//! GPU power readings: from `nvidia-smi` on any host, and on Apple Silicon
//! from IOReport through macmon. Ported from mesh-llm #2518.
//!
//! One `nvidia-smi` process streams a reading per GPU every second, and each
//! round is summed into one reading for the node. A round only counts when
//! every GPU in it reports power: a partial sum would understate the node, so
//! a round where a GPU misses its reading is skipped. A board that never
//! reports power (Jetson's integrated GPU answers `[N/A]`) stops the sampler
//! after its first round, and so does a macOS VM, whose GPU reads 0 W.

use anyhow::Result;
use tokio::io::{AsyncBufRead, Lines};
use tokio::sync::mpsc;

/// A node-wide reading: when, in Unix nanoseconds, and the watts.
pub type Reading = (u64, f64);

pub fn unix_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// One `index, power.draw` line of `nvidia-smi --format=csv,noheader,nounits`;
/// the watts are `None` for `[N/A]` and other non-readings.
fn parse_power_line(line: &str) -> Option<(u32, Option<f64>)> {
    let (index, watts) = line.split_once(',')?;
    let index = index.trim().parse().ok()?;
    let watts = watts
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|watts| watts.is_finite() && *watts >= 0.0);
    Some((index, watts))
}

#[derive(Debug)]
struct PowerRound {
    gpus: usize,
    seen: usize,
    watts: f64,
    missing: bool,
}

#[derive(Debug, PartialEq)]
enum Pushed {
    Pending,
    /// The node total, or `None` when a GPU in the round did not report.
    Complete(Option<f64>),
}

impl PowerRound {
    fn new(gpus: usize) -> Self {
        Self {
            gpus,
            seen: 0,
            watts: 0.0,
            missing: false,
        }
    }

    fn push(&mut self, watts: Option<f64>) -> Pushed {
        self.seen += 1;
        match watts {
            Some(watts) => self.watts += watts,
            None => self.missing = true,
        }
        if self.seen < self.gpus {
            return Pushed::Pending;
        }
        let total = (!self.missing).then_some(self.watts);
        *self = Self::new(self.gpus);
        Pushed::Complete(total)
    }
}

async fn read_rounds<R: AsyncBufRead + Unpin>(
    lines: &mut Lines<R>,
    gpus: usize,
    mut record: impl FnMut(f64),
) -> Result<()> {
    let mut round = PowerRound::new(gpus);
    let mut measured = false;
    while let Some(line) = lines.next_line().await? {
        let Some((_, watts)) = parse_power_line(&line) else {
            continue;
        };
        match round.push(watts) {
            Pushed::Pending => {}
            Pushed::Complete(Some(total)) => {
                measured = true;
                record(total);
            }
            Pushed::Complete(None) if !measured => {
                eprintln!(
                    "powermetrics-plugin: nvidia-smi does not report power draw for every GPU; no readings"
                );
                return Ok(());
            }
            Pushed::Complete(None) => {}
        }
    }
    Ok(())
}

/// Starts every source this machine has; each sends its readings and the name
/// it reports them under. Sources that are missing end quietly.
pub fn start(readings: mpsc::UnboundedSender<(&'static str, Reading)>) {
    #[cfg(target_os = "macos")]
    apple::start(readings.clone());
    tokio::spawn(async move {
        if let Err(error) = nvidia::sample(readings).await {
            eprintln!("powermetrics-plugin: nvidia-smi unavailable: {error:#}");
        }
    });
}

mod nvidia {
    use super::Reading;
    use anyhow::{Context, Result};
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;
    use tokio::sync::mpsc;

    const SOURCE: &str = "nvidia-smi";

    pub(super) async fn sample(
        readings: mpsc::UnboundedSender<(&'static str, Reading)>,
    ) -> Result<()> {
        let listing = Command::new("nvidia-smi")
            .args(["--query-gpu=index", "--format=csv,noheader"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .context("run nvidia-smi")?;
        let gpus = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if !listing.status.success() || gpus == 0 {
            return Ok(());
        }
        // `kill_on_drop` ends nvidia-smi with the plugin when it returns; a
        // plugin killed outright can leave it looping on Windows.
        let mut child = Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,power.draw",
                "--format=csv,noheader,nounits",
                "--loop-ms=1000",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("start nvidia-smi power sampling")?;
        let stdout = child
            .stdout
            .take()
            .context("nvidia-smi power sampling has no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        super::read_rounds(&mut lines, gpus, |total| {
            let _ = readings.send((SOURCE, (super::unix_now_ns(), total)));
        })
        .await
    }
}

/// What a macmon reading means for the sampler.
#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, PartialEq)]
enum MacmonReading {
    Record(f64),
    Skip,
    NoSource,
}

/// An Apple GPU never draws 0 W, but a VM without IOReport's GPU channels
/// (macOS CI runners) reads it so. A first 0 W reading means there is no
/// source, as a first missed round does for `nvidia-smi`; a later one is
/// skipped like a missed round.
#[cfg(any(test, target_os = "macos"))]
fn macmon_reading(gpu_power: f32, measured: &mut bool) -> MacmonReading {
    if gpu_power > 0.0 {
        *measured = true;
        MacmonReading::Record(f64::from(gpu_power))
    } else if *measured {
        MacmonReading::Skip
    } else {
        MacmonReading::NoSource
    }
}

/// Apple Silicon's GPU power from IOReport, read through macmon without sudo.
#[cfg(target_os = "macos")]
mod apple {
    use super::Reading;
    use tokio::sync::mpsc;

    const SOURCE: &str = "macmon";

    /// macmon blocks for each one-second reading, so it gets its own thread.
    /// A reading is the interval's average, stamped at its end. It ends
    /// quietly where macmon can't read the SoC (an Intel Mac), or reads the
    /// GPU at 0 W from the start (a VM).
    pub(super) fn start(readings: mpsc::UnboundedSender<(&'static str, Reading)>) {
        let spawned = std::thread::Builder::new()
            .name("powermetrics-macmon".into())
            .spawn(move || {
                let mut sampler = match macmon::Sampler::new() {
                    Ok(sampler) => sampler,
                    Err(error) => {
                        eprintln!("powermetrics-plugin: macmon unavailable: {error}");
                        return;
                    }
                };
                let mut measured = false;
                loop {
                    match sampler.get_metrics(1000) {
                        Ok(metrics) => {
                            match super::macmon_reading(metrics.gpu_power, &mut measured) {
                                super::MacmonReading::Record(watts) => {
                                    let _ = readings.send((SOURCE, (super::unix_now_ns(), watts)));
                                }
                                super::MacmonReading::Skip => {}
                                super::MacmonReading::NoSource => {
                                    eprintln!(
                                        "powermetrics-plugin: macmon reads the GPU at 0 W; no readings"
                                    );
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("powermetrics-plugin: macmon stopped: {error}");
                            return;
                        }
                    }
                }
            });
        if let Err(error) = spawned {
            eprintln!("powermetrics-plugin: macmon sampler not started: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MacmonReading, PowerRound, Pushed, macmon_reading, read_rounds};
    use tokio::io::AsyncBufReadExt;

    #[test]
    fn a_first_zero_watt_macmon_reading_means_no_source() {
        let mut measured = false;
        assert_eq!(macmon_reading(0.0, &mut measured), MacmonReading::NoSource);
        assert_eq!(
            macmon_reading(f32::NAN, &mut measured),
            MacmonReading::NoSource
        );
    }

    #[test]
    fn a_later_zero_watt_macmon_reading_is_skipped() {
        let mut measured = false;
        assert_eq!(
            macmon_reading(4.5, &mut measured),
            MacmonReading::Record(4.5)
        );
        assert_eq!(macmon_reading(0.0, &mut measured), MacmonReading::Skip);
        assert_eq!(
            macmon_reading(3.25, &mut measured),
            MacmonReading::Record(3.25)
        );
    }

    #[test]
    fn a_round_sums_its_gpus_once_every_one_reports_power() {
        let mut round = PowerRound::new(2);
        assert_eq!(round.push(Some(100.0)), Pushed::Pending);
        assert_eq!(round.push(Some(50.0)), Pushed::Complete(Some(150.0)));
    }

    #[test]
    fn a_round_where_a_gpu_misses_its_reading_has_no_reading() {
        let mut round = PowerRound::new(2);
        assert_eq!(round.push(None), Pushed::Pending);
        assert_eq!(round.push(Some(40.0)), Pushed::Complete(None));
    }

    async fn recorded(output: &str, gpus: usize) -> Vec<f64> {
        let mut lines = output.as_bytes().lines();
        let mut totals = Vec::new();
        read_rounds(&mut lines, gpus, |total| totals.push(total))
            .await
            .unwrap();
        totals
    }

    #[tokio::test]
    async fn a_board_without_power_readings_stops_after_one_round() {
        assert!(
            recorded("0, [N/A]\n0, [N/A]\n0, 30.0\n", 1)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_later_round_with_a_missing_reading_is_skipped() {
        let output = "0, 120.0\n1, 5.0\n0, 115.0\n1, [N/A]\n0, 110.0\n1, 20.0\n";
        assert_eq!(recorded(output, 2).await, [125.0, 130.0]);
    }
}
