//! Energy a node spends serving, from its GPU power readings and the local
//! inference observation feed. Ported from mesh-llm #2518's host meter.
//!
//! Each interval between two power samples is counted as busy when an
//! execution ran during any part of it, and as idle otherwise. That gives the
//! idle draw, which keeping the node on costs whether or not it serves, and
//! the energy per 1,000 output tokens on top of that idle draw.
//!
//! The per-token figures only use executions whose whole run was measured:
//! each busy interval's energy is shared among the executions that ran in it,
//! and an execution's tokens count together with its share, or not at all
//! when part of its run fell in a sampling gap, after the sampler went quiet,
//! or in a stretch of the feed that was lost. Tokens and energy therefore
//! always describe the same work.

use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// The host's id for one execution, from the observation feed.
pub type ExecutionId = u64;

/// Samples further apart than this leave a gap rather than an interval: the
/// sampler stalled or restarted, and nothing is known about the time between.
const MAX_SAMPLE_GAP_SECONDS: f64 = 10.0;
/// Idle draw is reported once this much idle time has been measured.
const MIN_IDLE_SECONDS: f64 = 5.0;

/// One request's share of the busy energy, kept until it can be rated.
#[derive(Debug, Default)]
struct RequestEnergy {
    joules: f64,
    seconds: f64,
    /// Output tokens, from whichever event reports them first.
    tokens: Option<u64>,
    /// Part of its run was not measured: a sampling gap, or no recent sample
    /// when it started or ended.
    unmeasured: bool,
    /// The backend is still computing for it.
    computing: bool,
    /// Its terminal event arrived.
    closed: bool,
}

/// Accumulates power samples and request activity into busy and idle energy.
#[derive(Debug, Default)]
pub struct EnergyMeter {
    last_sample: Option<(f64, f64)>,
    requests: HashMap<ExecutionId, RequestEnergy>,
    /// Requests that ran during the interval since the last sample.
    active: HashSet<ExecutionId>,
    busy_seconds: f64,
    idle_joules: f64,
    idle_seconds: f64,
    /// Energy, busy time and tokens of the fully measured requests only.
    rated_joules: f64,
    rated_seconds: f64,
    output_tokens: u64,
}

/// What [`EnergyMeter`] has measured so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EnergyStatusPayload {
    /// Where power readings come from, such as `nvidia-smi`.
    pub source: &'static str,
    /// Mean board power while no request ran, once enough idle time is measured.
    pub idle_watts: Option<f64>,
    /// Energy per 1,000 output tokens on top of the idle draw.
    pub wh_per_1k_output_tokens: Option<f64>,
    /// Energy per 1,000 output tokens including the idle draw during requests.
    pub wh_per_1k_output_tokens_total: Option<f64>,
    /// Output tokens of the requests whose whole run was measured. Both
    /// per-token rates divide those requests' share of the busy energy by these.
    pub output_tokens: u64,
    /// All measured busy time, including requests left out of the rates.
    pub busy_seconds: f64,
    pub idle_seconds: f64,
}

impl EnergyMeter {
    /// One power reading, `watts` summed over the GPUs, at `seconds` on a
    /// monotonic clock.
    pub fn record_power(&mut self, seconds: f64, watts: f64) {
        if !watts.is_finite() || watts < 0.0 {
            return;
        }
        if let Some((previous_seconds, previous_watts)) = self.last_sample {
            let elapsed = seconds - previous_seconds;
            if elapsed <= 0.0 {
                return;
            }
            if elapsed <= MAX_SAMPLE_GAP_SECONDS {
                let joules = (previous_watts + watts) / 2.0 * elapsed;
                if self.active.is_empty() {
                    self.idle_joules += joules;
                    self.idle_seconds += elapsed;
                } else {
                    self.busy_seconds += elapsed;
                    let share = 1.0 / self.active.len() as f64;
                    for request in &self.active {
                        if let Some(energy) = self.requests.get_mut(request) {
                            energy.joules += joules * share;
                            energy.seconds += elapsed * share;
                        }
                    }
                }
            } else {
                // Nothing is known about the gap, so neither is what the
                // requests running through it cost.
                for request in &self.active {
                    if let Some(energy) = self.requests.get_mut(request) {
                        energy.unmeasured = true;
                    }
                }
            }
        }
        self.last_sample = Some((seconds, watts));
        // A request that stopped computing has had its last interval counted.
        let settled: Vec<ExecutionId> = self
            .active
            .iter()
            .filter(|request| {
                self.requests
                    .get(request)
                    .is_none_or(|energy| !energy.computing)
            })
            .copied()
            .collect();
        for request in settled {
            self.active.remove(&request);
            if self
                .requests
                .get(&request)
                .is_some_and(|energy| energy.closed)
            {
                self.rate(&request);
            }
        }
    }

    pub fn request_started(&mut self, request: ExecutionId, seconds: f64) {
        let measured = self.sampled_recently(seconds);
        let energy = self.requests.entry(request).or_default();
        energy.computing = true;
        energy.unmeasured |= !measured;
        self.active.insert(request);
    }

    /// The backend stopped computing for `request`. The interval this happens
    /// in stays busy: part of it was.
    pub fn compute_finished(&mut self, request: &ExecutionId) {
        if let Some(energy) = self.requests.get_mut(request) {
            energy.computing = false;
        }
    }

    /// `request` is over: no later event can report its tokens again. It is
    /// rated once the interval it ended in is counted, which the next sample
    /// does; with no recent sample, none may come, and it is left out.
    pub fn request_closed(&mut self, request: &ExecutionId, seconds: f64) {
        let measured = self.sampled_recently(seconds);
        let Some(energy) = self.requests.get_mut(request) else {
            return;
        };
        energy.computing = false;
        energy.closed = true;
        energy.unmeasured |= !measured;
        if !measured || !self.active.contains(request) {
            self.active.remove(request);
            self.rate(request);
        }
    }

    /// Keeps a request's output tokens, whichever event reports them first.
    pub fn tokens_completed(&mut self, request: ExecutionId, completion_tokens: u64) {
        if let Some(energy) = self.requests.get_mut(&request) {
            energy.tokens.get_or_insert(completion_tokens);
        }
    }

    /// Adds a finished request to the rates if its whole run was measured.
    fn rate(&mut self, request: &ExecutionId) {
        let Some(energy) = self.requests.remove(request) else {
            return;
        };
        if let Some(tokens) = energy.tokens
            && !energy.unmeasured
        {
            self.rated_joules += energy.joules;
            self.rated_seconds += energy.seconds;
            self.output_tokens += tokens;
        }
    }

    fn sampled_recently(&self, seconds: f64) -> bool {
        self.last_sample
            .is_some_and(|(sampled, _)| seconds - sampled <= MAX_SAMPLE_GAP_SECONDS)
    }

    pub fn status(&self, source: &'static str) -> EnergyStatusPayload {
        let idle_watts =
            (self.idle_seconds >= MIN_IDLE_SECONDS).then(|| self.idle_joules / self.idle_seconds);
        let per_1k_tokens = |joules: f64| {
            (self.output_tokens > 0).then(|| joules / 3600.0 / (self.output_tokens as f64 / 1000.0))
        };
        EnergyStatusPayload {
            source,
            idle_watts,
            wh_per_1k_output_tokens: idle_watts.and_then(|idle| {
                per_1k_tokens((self.rated_joules - idle * self.rated_seconds).max(0.0))
            }),
            wh_per_1k_output_tokens_total: per_1k_tokens(self.rated_joules),
            output_tokens: self.output_tokens,
            busy_seconds: self.busy_seconds,
            idle_seconds: self.idle_seconds,
        }
    }
}

impl EnergyMeter {
    /// The feed lost events: what ran through the loss can't be rated, so
    /// every execution running now is left out of the rates.
    pub fn lose_track_of_running(&mut self) {
        for energy in self.requests.values_mut() {
            energy.unmeasured = true;
        }
    }

    /// Executions a feed snapshot says are running, which this meter may not
    /// have seen start: they make intervals busy, but are never rated.
    pub fn running_since_unknown(
        &mut self,
        executions: impl IntoIterator<Item = (ExecutionId, bool)>,
    ) {
        for (execution, computing) in executions {
            let energy = self.requests.entry(execution).or_default();
            energy.unmeasured = true;
            energy.computing |= computing;
            if computing {
                self.active.insert(execution);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "test";

    /// One sample a second from `from` to `to` inclusive, all at `watts`.
    fn samples(meter: &mut EnergyMeter, from: u32, to: u32, watts: f64) {
        for second in from..=to {
            meter.record_power(f64::from(second), watts);
        }
    }

    #[test]
    fn idle_draw_needs_enough_idle_time_and_no_tokens_means_no_rate() {
        let mut meter = EnergyMeter::default();
        samples(&mut meter, 0, 3, 10.0);
        assert_eq!(meter.status(SOURCE).idle_watts, None);
        samples(&mut meter, 4, 10, 10.0);
        let status = meter.status(SOURCE);
        assert_eq!(status.idle_watts, Some(10.0));
        assert_eq!(status.idle_seconds, 10.0);
        assert_eq!(status.wh_per_1k_output_tokens, None);
        assert_eq!(status.wh_per_1k_output_tokens_total, None);
    }

    #[test]
    fn serving_energy_is_split_into_idle_draw_and_what_tokens_add() {
        let mut meter = EnergyMeter::default();
        let request = 1;
        samples(&mut meter, 0, 10, 10.0);
        meter.request_started(request, 10.0);
        // [10, 11] averages 10 W and 110 W; then nine seconds at 110 W.
        samples(&mut meter, 11, 19, 110.0);
        meter.tokens_completed(request, 1_000);
        meter.request_closed(&request, 19.5);
        // The interval it ended in is counted, then it is rated.
        meter.record_power(20.0, 110.0);

        let status = meter.status(SOURCE);
        assert_eq!(status.idle_watts, Some(10.0));
        assert_eq!(status.busy_seconds, 10.0);
        assert_eq!(status.output_tokens, 1_000);
        let total = status.wh_per_1k_output_tokens_total.unwrap();
        let above_idle = status.wh_per_1k_output_tokens.unwrap();
        assert!((total - 1_050.0 / 3_600.0).abs() < 1e-9, "{total}");
        assert!((above_idle - 950.0 / 3_600.0).abs() < 1e-9, "{above_idle}");
    }

    #[test]
    fn an_interval_a_request_touched_counts_as_busy() {
        let mut meter = EnergyMeter::default();
        let request = 2;
        meter.record_power(0.0, 50.0);
        meter.request_started(request, 0.2);
        meter.request_closed(&request, 0.3);
        meter.record_power(1.0, 50.0);
        meter.record_power(2.0, 50.0);
        let status = meter.status(SOURCE);
        assert_eq!(status.busy_seconds, 1.0);
        assert_eq!(status.idle_seconds, 1.0);
    }

    #[test]
    fn gaps_and_impossible_readings_are_not_integrated() {
        let mut meter = EnergyMeter::default();
        meter.record_power(0.0, 10.0);
        meter.record_power(60.0, 10.0);
        meter.record_power(61.0, f64::NAN);
        meter.record_power(61.0, -5.0);
        meter.record_power(62.0, 10.0);
        assert_eq!(meter.status(SOURCE).idle_seconds, 2.0);
    }

    /// A rate from one request: 1,000 tokens over ten seconds at 110 W, after ten
    /// idle seconds at 10 W.
    fn meter_with_a_rate() -> EnergyMeter {
        let mut meter = EnergyMeter::default();
        let request = 3;
        samples(&mut meter, 0, 10, 10.0);
        meter.request_started(request, 10.0);
        samples(&mut meter, 11, 19, 110.0);
        meter.tokens_completed(request, 1_000);
        meter.request_closed(&request, 19.5);
        meter.record_power(20.0, 110.0);
        meter
    }

    #[test]
    fn a_long_stream_stays_busy_until_its_terminal_event() {
        // 4,096 tokens at 2 tokens/s run well past half an hour.
        let mut meter = EnergyMeter::default();
        let stream = 4;
        samples(&mut meter, 0, 10, 10.0);
        meter.request_started(stream, 10.0);
        samples(&mut meter, 11, 2_058, 110.0);
        meter.tokens_completed(stream, 4_096);
        meter.request_closed(&stream, 2_058.5);
        meter.record_power(2_059.0, 110.0);
        let status = meter.status(SOURCE);
        assert_eq!(status.idle_seconds, 10.0);
        assert_eq!(status.busy_seconds, 2_049.0);
        assert_eq!(status.output_tokens, 4_096);
    }

    #[test]
    fn tokens_produced_after_the_sampler_stops_leave_the_rate_alone() {
        let mut meter = meter_with_a_rate();
        let before = meter.status(SOURCE);
        // The sampler's last reading was at 20 s; these requests run without any.
        for start in [30.0, 60.0, 90.0] {
            let request = 5;
            meter.request_started(request, start);
            meter.tokens_completed(request, 5_000);
            meter.request_closed(&request, start + 20.0);
        }
        assert_eq!(meter.status(SOURCE), before);
        assert!(meter.requests.is_empty() && meter.active.is_empty());
    }

    #[test]
    fn a_request_running_through_a_sampling_gap_is_left_out_of_the_rate() {
        let mut meter = meter_with_a_rate();
        let before = meter.status(SOURCE);
        let request = 6;
        meter.request_started(request, 20.5);
        meter.record_power(21.0, 110.0);
        // The sampler stalls past the gap limit, then resumes.
        meter.record_power(40.0, 110.0);
        samples(&mut meter, 41, 45, 110.0);
        meter.tokens_completed(request, 2_000);
        meter.request_closed(&request, 45.5);
        meter.record_power(46.0, 110.0);
        let after = meter.status(SOURCE);
        assert_eq!(after.output_tokens, before.output_tokens);
        assert_eq!(
            after.wh_per_1k_output_tokens,
            before.wh_per_1k_output_tokens
        );
        assert_eq!(
            after.wh_per_1k_output_tokens_total,
            before.wh_per_1k_output_tokens_total
        );
        // Its measured intervals still count as busy time.
        assert_eq!(after.busy_seconds, before.busy_seconds + 7.0);
    }

    #[test]
    fn concurrent_requests_share_the_energy_of_the_intervals_they_ran_in() {
        let mut meter = EnergyMeter::default();
        samples(&mut meter, 0, 10, 10.0);
        let first = 7;
        let second = 8;
        meter.request_started(first, 10.0);
        meter.request_started(second, 10.0);
        samples(&mut meter, 11, 20, 210.0);
        meter.tokens_completed(first, 300);
        meter.tokens_completed(second, 700);
        meter.request_closed(&first, 20.5);
        meter.request_closed(&second, 20.5);
        meter.record_power(21.0, 210.0);
        let status = meter.status(SOURCE);
        assert_eq!(status.output_tokens, 1_000);
        // [10, 11] averages 10 W and 210 W, then ten seconds at 210 W: 2,210 J for
        // the 1,000 tokens of both requests together.
        let total = status.wh_per_1k_output_tokens_total.unwrap();
        assert!((total - 2_210.0 / 3_600.0).abs() < 1e-9, "{total}");
    }

    #[test]
    fn a_loss_leaves_what_was_running_out_of_the_rates() {
        let mut meter = meter_with_a_rate();
        let before = meter.status(SOURCE);
        meter.request_started(77, 20.5);
        meter.record_power(21.0, 110.0);
        meter.lose_track_of_running();
        meter.tokens_completed(77, 2_000);
        meter.request_closed(&77, 21.5);
        meter.record_power(22.0, 110.0);
        assert_eq!(meter.status(SOURCE).output_tokens, before.output_tokens);
    }

    #[test]
    fn executions_from_a_snapshot_keep_the_node_busy_but_are_never_rated() {
        let mut meter = EnergyMeter::default();
        samples(&mut meter, 0, 10, 10.0);
        meter.running_since_unknown([(5, true)]);
        samples(&mut meter, 11, 15, 110.0);
        meter.tokens_completed(5, 500);
        meter.request_closed(&5, 15.5);
        meter.record_power(16.0, 110.0);
        let status = meter.status(SOURCE);
        assert_eq!(status.busy_seconds, 6.0);
        assert_eq!(status.output_tokens, 0);
    }
}
