//! AMD GPU power, for machines where `nvidia-smi` finds no GPU: on Windows
//! from ADLX, the library the Adrenalin driver installs (`amdadlx64.dll`), and
//! on Linux from the amdgpu driver's hwmon files. Each round sums the AMD GPUs
//! that report power, once a second. On an APU the driver's figure is the
//! integrated GPU's own rail where it reports one, which can still include
//! some shared SoC power.

use super::Reading;
use tokio::sync::mpsc;

/// Starts the AMD source on its own thread, since both readers block. It ends
/// quietly where there is no AMD GPU power to read.
pub(super) fn start(readings: mpsc::UnboundedSender<(&'static str, Reading)>) {
    #[cfg(any(windows, target_os = "linux"))]
    {
        let spawned = std::thread::Builder::new()
            .name("powermetrics-amd".into())
            .spawn(move || run(readings));
        if let Err(error) = spawned {
            eprintln!("powermetrics-plugin: AMD sampler not started: {error}");
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    drop(readings);
}

/// A first round without power means there is no source, as for
/// `nvidia-smi`; a later one is skipped.
#[cfg(any(windows, target_os = "linux"))]
fn run(readings: mpsc::UnboundedSender<(&'static str, Reading)>) {
    #[cfg(windows)]
    let (source, mut read) = match adlx::Adlx::open() {
        Ok(Some(adlx)) => (
            "adlx",
            Box::new(move || adlx.read()) as Box<dyn FnMut() -> Option<f64>>,
        ),
        Ok(None) => return,
        Err(error) => {
            eprintln!("powermetrics-plugin: ADLX unavailable: {error:#}");
            return;
        }
    };
    #[cfg(target_os = "linux")]
    let (source, mut read) = {
        let files = hwmon::power_files(std::path::Path::new("/sys/class/drm"));
        if files.is_empty() {
            return;
        }
        (
            "amdgpu",
            Box::new(move || hwmon::read_watts(&files)) as Box<dyn FnMut() -> Option<f64>>,
        )
    };
    let mut measured = false;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        match read() {
            Some(watts) => {
                measured = true;
                if readings
                    .send((source, (super::unix_now_ns(), watts)))
                    .is_err()
                {
                    return;
                }
            }
            None if !measured => {
                eprintln!("powermetrics-plugin: {source} does not report GPU power; no readings");
                return;
            }
            None => {}
        }
    }
}

#[cfg(windows)]
mod adlx {
    use anyhow::{Context, Result, bail};
    use libloading::Library;
    use std::ffi::c_void;

    type Object = *mut c_void;
    const OK: i32 = 0;

    // Vtable slots, from GPUOpen's ADLX SDK headers (`ISystem.h`,
    // `IPerformanceMonitoring.h`). IADLXSystem has no Acquire/Release; every
    // other interface starts with Acquire, Release, QueryInterface.
    const RELEASE: usize = 1;
    const SYSTEM_GET_GPUS: usize = 1;
    const SYSTEM_GET_PERFORMANCE_MONITORING_SERVICES: usize = 9;
    const LIST_SIZE: usize = 3;
    const GPU_LIST_AT: usize = 11;
    const PERFORMANCE_GET_CURRENT_GPU_METRICS: usize = 18;
    const PERFORMANCE_GET_SUPPORTED_GPU_METRICS: usize = 21;
    const SUPPORT_IS_SUPPORTED_GPU_POWER: usize = 8;
    const METRICS_GPU_POWER: usize = 9;

    /// The function at `index` in `object`'s vtable.
    ///
    /// # Safety
    /// `object` is a live ADLX interface whose vtable has that slot.
    unsafe fn slot(object: Object, index: usize) -> *const c_void {
        unsafe { *(*(object as *const *const *const c_void)).add(index) }
    }

    unsafe fn release(object: Object) {
        let release: unsafe extern "system" fn(Object) -> i32 =
            unsafe { std::mem::transmute(slot(object, RELEASE)) };
        unsafe { release(object) };
    }

    /// One `(pThis, ..., Object*)` getter that hands back a new interface.
    unsafe fn get(object: Object, index: usize, argument: Option<*mut c_void>) -> Option<Object> {
        let mut out: Object = std::ptr::null_mut();
        let result = match argument {
            None => {
                let getter: unsafe extern "system" fn(Object, *mut Object) -> i32 =
                    unsafe { std::mem::transmute(slot(object, index)) };
                unsafe { getter(object, &mut out) }
            }
            Some(argument) => {
                let getter: unsafe extern "system" fn(Object, *mut c_void, *mut Object) -> i32 =
                    unsafe { std::mem::transmute(slot(object, index)) };
                unsafe { getter(object, argument, &mut out) }
            }
        };
        (result == OK && !out.is_null()).then_some(out)
    }

    /// ADLX with the performance-monitoring service and the GPUs that report
    /// power. Used from the one thread that opened it.
    pub(super) struct Adlx {
        library: Library,
        performance: Object,
        gpus: Vec<Object>,
    }

    impl Adlx {
        /// `None` when ADLX loads but no GPU reports power.
        pub(super) fn open() -> Result<Option<Self>> {
            // SAFETY: the system DLL the AMD driver installs, looked up in
            // System32 only so a same-named file next to the plugin is never
            // loaded instead; its exports are called with the signatures the
            // ADLX headers declare.
            let library: Library = unsafe {
                libloading::os::windows::Library::load_with_flags(
                    "amdadlx64.dll",
                    libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            }
            .context("load amdadlx64.dll from System32")?
            .into();
            let mut version = 0u64;
            let mut system: Object = std::ptr::null_mut();
            unsafe {
                let query = library
                    .get::<unsafe extern "C" fn(*mut u64) -> i32>(b"ADLXQueryFullVersion\0")?;
                let initialize = library
                    .get::<unsafe extern "C" fn(u64, *mut Object) -> i32>(b"ADLXInitialize\0")?;
                if query(&mut version) != OK
                    || initialize(version, &mut system) != OK
                    || system.is_null()
                {
                    bail!("ADLX did not initialize");
                }
            }
            let mut adlx = Self {
                library,
                performance: std::ptr::null_mut(),
                gpus: Vec::new(),
            };
            unsafe {
                adlx.performance = get(system, SYSTEM_GET_PERFORMANCE_MONITORING_SERVICES, None)
                    .context("ADLX performance monitoring")?;
                let list = get(system, SYSTEM_GET_GPUS, None).context("ADLX GPU list")?;
                let size: unsafe extern "system" fn(Object) -> u32 =
                    std::mem::transmute(slot(list, LIST_SIZE));
                let at: unsafe extern "system" fn(Object, u32, *mut Object) -> i32 =
                    std::mem::transmute(slot(list, GPU_LIST_AT));
                for index in 0..size(list) {
                    let mut gpu: Object = std::ptr::null_mut();
                    if at(list, index, &mut gpu) != OK || gpu.is_null() {
                        continue;
                    }
                    if adlx.reports_power(gpu) {
                        adlx.gpus.push(gpu);
                    } else {
                        release(gpu);
                    }
                }
                release(list);
            }
            Ok((!adlx.gpus.is_empty()).then_some(adlx))
        }

        unsafe fn reports_power(&self, gpu: Object) -> bool {
            let Some(support) = (unsafe {
                get(
                    self.performance,
                    PERFORMANCE_GET_SUPPORTED_GPU_METRICS,
                    Some(gpu),
                )
            }) else {
                return false;
            };
            let mut supported = false;
            let is_supported: unsafe extern "system" fn(Object, *mut bool) -> i32 =
                unsafe { std::mem::transmute(slot(support, SUPPORT_IS_SUPPORTED_GPU_POWER)) };
            let result = unsafe { is_supported(support, &mut supported) };
            unsafe { release(support) };
            result == OK && supported
        }

        /// The summed GPU power, or `None` when a GPU missed its reading.
        pub(super) fn read(&self) -> Option<f64> {
            let mut total = 0.0;
            for &gpu in &self.gpus {
                // SAFETY: `performance` and `gpu` stay acquired until drop.
                let metrics = unsafe {
                    get(
                        self.performance,
                        PERFORMANCE_GET_CURRENT_GPU_METRICS,
                        Some(gpu),
                    )
                }?;
                let mut watts = 0.0f64;
                let power: unsafe extern "system" fn(Object, *mut f64) -> i32 =
                    unsafe { std::mem::transmute(slot(metrics, METRICS_GPU_POWER)) };
                let result = unsafe { power(metrics, &mut watts) };
                unsafe { release(metrics) };
                if result != OK || !watts.is_finite() || watts < 0.0 {
                    return None;
                }
                total += watts;
            }
            Some(total)
        }
    }

    impl Drop for Adlx {
        fn drop(&mut self) {
            // SAFETY: each object was acquired by `open` and is released once.
            unsafe {
                for &gpu in &self.gpus {
                    release(gpu);
                }
                if !self.performance.is_null() {
                    release(self.performance);
                }
                if let Ok(terminate) = self
                    .library
                    .get::<unsafe extern "C" fn() -> i32>(b"ADLXTerminate\0")
                {
                    terminate();
                }
            }
        }
    }
}

#[cfg(any(test, target_os = "linux"))]
mod hwmon {
    use std::path::{Path, PathBuf};

    const AMD_VENDOR: &str = "0x1002";

    /// The power file of each amdgpu card under `drm` (normally
    /// `/sys/class/drm`): `cardN/device/hwmon/hwmon*/power1_average`, or
    /// `power1_input` where the kernel exposes that one instead. Connector
    /// entries (`card0-DP-1`) and other vendors' cards are skipped.
    pub(super) fn power_files(drm: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(drm) else {
            return Vec::new();
        };
        let mut cards: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.strip_prefix("card").is_some_and(|index| {
                    !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit())
                })
            })
            .map(|entry| entry.path().join("device"))
            .filter(|device| {
                std::fs::read_to_string(device.join("vendor"))
                    .is_ok_and(|vendor| vendor.trim() == AMD_VENDOR)
            })
            .collect();
        cards.sort();
        cards
            .into_iter()
            .filter_map(|device| {
                let mut monitors: Vec<PathBuf> = std::fs::read_dir(device.join("hwmon"))
                    .ok()?
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .collect();
                monitors.sort();
                monitors.into_iter().find_map(|monitor| {
                    ["power1_average", "power1_input"]
                        .into_iter()
                        .map(|name| monitor.join(name))
                        .find(|file| file.is_file())
                })
            })
            .collect()
    }

    /// The summed power of `files`, in watts (the files hold microwatts), or
    /// `None` when one of them can't be read.
    pub(super) fn read_watts(files: &[PathBuf]) -> Option<f64> {
        files.iter().try_fold(0.0, |total, file| {
            let microwatts: f64 = std::fs::read_to_string(file).ok()?.trim().parse().ok()?;
            (microwatts.is_finite() && microwatts >= 0.0).then(|| total + microwatts / 1_000_000.0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::hwmon::{power_files, read_watts};
    use std::fs;
    use std::path::Path;

    fn card(drm: &Path, name: &str, vendor: &str, file: Option<(&str, &str)>) {
        let device = drm.join(name).join("device");
        fs::create_dir_all(device.join("hwmon").join("hwmon3")).unwrap();
        fs::write(device.join("vendor"), format!("{vendor}\n")).unwrap();
        if let Some((file, microwatts)) = file {
            fs::write(
                device.join("hwmon").join("hwmon3").join(file),
                format!("{microwatts}\n"),
            )
            .unwrap();
        }
    }

    fn drm(test: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("powermetrics-amd-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn sums_amd_cards_and_skips_connectors_and_other_vendors() {
        let root = drm("sum");
        card(
            &root,
            "card0",
            "0x1002",
            Some(("power1_average", "15000000")),
        );
        card(
            &root,
            "card1",
            "0x10de",
            Some(("power1_average", "200000000")),
        );
        card(&root, "card2", "0x1002", Some(("power1_input", "2500000")));
        card(
            &root,
            "card0-DP-1",
            "0x1002",
            Some(("power1_average", "99000000")),
        );
        let files = power_files(&root);
        assert_eq!(files.len(), 2);
        assert_eq!(read_watts(&files), Some(17.5));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_amd_card_without_a_power_file_is_not_a_source() {
        let root = drm("none");
        card(&root, "card0", "0x1002", None);
        assert!(power_files(&root).is_empty());
        assert!(power_files(&root.join("missing")).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreadable_value_misses_the_whole_round() {
        let root = drm("bad");
        card(
            &root,
            "card0",
            "0x1002",
            Some(("power1_average", "12000000")),
        );
        card(&root, "card1", "0x1002", Some(("power1_average", "n/a")));
        assert_eq!(read_watts(&power_files(&root)), None);
        fs::remove_dir_all(root).unwrap();
    }

    /// Needs an AMD GPU and the Adrenalin driver:
    /// `cargo test -- --ignored --nocapture reads_gpu_power_from_adlx`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn reads_gpu_power_from_adlx() {
        let adlx = super::adlx::Adlx::open()
            .expect("ADLX loads")
            .expect("a GPU reports power");
        for _ in 0..10 {
            let watts = adlx.read().expect("a reading");
            println!("ADLX GPU power: {watts:.1} W");
            assert!(watts > 0.0);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
