//! AMD GPU power, for machines where `nvidia-smi` finds no GPU: on Windows
//! from ADLX, the library the Adrenalin driver installs (`amdadlx64.dll`), and
//! on Linux from the amdgpu driver's hwmon files. Each round sums the AMD GPUs
//! that report power, once a second.
//!
//! An APU's figure includes its CPU cores (the kernel documents it for hwmon,
//! and ADLX follows a CPU-only load), so it is published under its own source,
//! `adlx-apu` or `amdgpu-apu`. Where a discrete AMD GPU reports power, only the
//! discrete GPUs are read, under `adlx` or `amdgpu`.

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
            if adlx.apu { "adlx-apu" } else { "adlx" },
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
        let cards = hwmon::power_files(std::path::Path::new("/sys/class/drm"));
        let Some((files, apu)) = hwmon::choose(cards) else {
            return;
        };
        (
            if apu { "amdgpu-apu" } else { "amdgpu" },
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
    const GPU_TYPE: usize = 5;
    const SUPPORT_IS_SUPPORTED_GPU_POWER: usize = 8;
    const SUPPORT_IS_SUPPORTED_GPU_TOTAL_BOARD_POWER: usize = 9;
    const METRICS_GPU_POWER: usize = 9;
    const METRICS_GPU_TOTAL_BOARD_POWER: usize = 10;
    /// `GPUTYPE_INTEGRATED` in `ADLX_GPU_TYPE`.
    const GPU_TYPE_INTEGRATED: i32 = 1;

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

    /// Whether ADLX calls `gpu` integrated; an unknown type counts as discrete.
    ///
    /// # Safety
    /// `gpu` is a live IADLXGPU.
    unsafe fn is_integrated(gpu: Object) -> bool {
        let mut kind = 0i32;
        let get_type: unsafe extern "system" fn(Object, *mut i32) -> i32 =
            unsafe { std::mem::transmute(slot(gpu, GPU_TYPE)) };
        (unsafe { get_type(gpu, &mut kind) }) == OK && kind == GPU_TYPE_INTEGRATED
    }

    /// ADLX with the performance-monitoring service and the GPUs it reads,
    /// each with the metric slot it reads. Used from the one thread that
    /// opened it.
    pub(super) struct Adlx {
        library: Library,
        performance: Object,
        gpus: Vec<(Object, usize)>,
        /// The GPUs read are integrated ones, whose figure includes the CPU.
        pub(super) apu: bool,
    }

    impl Adlx {
        /// `None` when ADLX loads but no GPU reports power.
        pub(super) fn open() -> Result<Option<Self>> {
            // SAFETY: the system DLL the AMD driver installs, looked up in
            // System32 only so a same-named file next to the plugin is never
            // loaded instead; its exports are called with the signatures the
            // ADLX headers declare.
            let Ok(library) = (unsafe {
                libloading::os::windows::Library::load_with_flags(
                    "amdadlx64.dll",
                    libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            }) else {
                // No AMD driver: a missing source ends quietly.
                return Ok(None);
            };
            let library: Library = library.into();
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
                apu: false,
            };
            unsafe {
                adlx.performance = get(system, SYSTEM_GET_PERFORMANCE_MONITORING_SERVICES, None)
                    .context("ADLX performance monitoring")?;
                let list = get(system, SYSTEM_GET_GPUS, None).context("ADLX GPU list")?;
                let size: unsafe extern "system" fn(Object) -> u32 =
                    std::mem::transmute(slot(list, LIST_SIZE));
                let at: unsafe extern "system" fn(Object, u32, *mut Object) -> i32 =
                    std::mem::transmute(slot(list, GPU_LIST_AT));
                let mut discrete = Vec::new();
                let mut integrated = Vec::new();
                for index in 0..size(list) {
                    let mut gpu: Object = std::ptr::null_mut();
                    if at(list, index, &mut gpu) != OK || gpu.is_null() {
                        continue;
                    }
                    match adlx.power_metric(gpu) {
                        Some(metric) if is_integrated(gpu) => integrated.push((gpu, metric)),
                        Some(metric) => discrete.push((gpu, metric)),
                        None => release(gpu),
                    }
                }
                release(list);
                // The discrete GPUs alone when there are any: an integrated
                // GPU's figure is mostly the CPU's.
                if discrete.is_empty() {
                    adlx.apu = !integrated.is_empty();
                    adlx.gpus = integrated;
                } else {
                    for (gpu, _) in integrated {
                        release(gpu);
                    }
                    adlx.gpus = discrete;
                }
            }
            Ok((!adlx.gpus.is_empty()).then_some(adlx))
        }

        /// The metric slot to read for `gpu`: total board power where the
        /// driver reports it, like `nvidia-smi`'s `power.draw`, else the GPU's
        /// own power; `None` when it reports neither.
        unsafe fn power_metric(&self, gpu: Object) -> Option<usize> {
            let support = unsafe {
                get(
                    self.performance,
                    PERFORMANCE_GET_SUPPORTED_GPU_METRICS,
                    Some(gpu),
                )
            }?;
            let supports = |index: usize| {
                let mut supported = false;
                let is_supported: unsafe extern "system" fn(Object, *mut bool) -> i32 =
                    unsafe { std::mem::transmute(slot(support, index)) };
                (unsafe { is_supported(support, &mut supported) }) == OK && supported
            };
            let metric = if supports(SUPPORT_IS_SUPPORTED_GPU_TOTAL_BOARD_POWER) {
                Some(METRICS_GPU_TOTAL_BOARD_POWER)
            } else if supports(SUPPORT_IS_SUPPORTED_GPU_POWER) {
                Some(METRICS_GPU_POWER)
            } else {
                None
            };
            unsafe { release(support) };
            metric
        }

        /// The summed GPU power, or `None` when a GPU missed its reading.
        pub(super) fn read(&self) -> Option<f64> {
            let mut total = 0.0;
            for &(gpu, metric) in &self.gpus {
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
                    unsafe { std::mem::transmute(slot(metrics, metric)) };
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
                for &(gpu, _) in &self.gpus {
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
    /// `power1_input` where the kernel exposes that one instead, and whether
    /// the card is an APU's. Connector entries (`card0-DP-1`) and other
    /// vendors' cards are skipped.
    pub(super) fn power_files(drm: &Path) -> Vec<(PathBuf, bool)> {
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
                    let file = ["power1_average", "power1_input"]
                        .into_iter()
                        .map(|name| monitor.join(name))
                        .find(|file| file.is_file())?;
                    Some((file, is_apu(&monitor)))
                })
            })
            .collect()
    }

    /// amdgpu shows the northbridge voltage, labelled `vddnb`, on APUs only
    /// (`amdgpu_hwmon_attributes_visible` in the kernel's `amdgpu_pm.c`).
    fn is_apu(monitor: &Path) -> bool {
        std::fs::read_to_string(monitor.join("in1_label"))
            .is_ok_and(|label| label.trim() == "vddnb")
    }

    /// The files to read and whether they are APUs': the discrete cards when
    /// there are any, else the APUs. `None` when there is no card.
    pub(super) fn choose(cards: Vec<(PathBuf, bool)>) -> Option<(Vec<PathBuf>, bool)> {
        let (apus, discrete): (Vec<_>, Vec<_>) = cards.into_iter().partition(|(_, apu)| *apu);
        let apu = discrete.is_empty();
        let files: Vec<PathBuf> = if apu { apus } else { discrete }
            .into_iter()
            .map(|(file, _)| file)
            .collect();
        (!files.is_empty()).then_some((files, apu))
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
    use super::hwmon::{choose, power_files, read_watts};
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
        let (files, apu) = choose(power_files(&root)).unwrap();
        assert!(!apu);
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
        assert_eq!(read_watts(&choose(power_files(&root)).unwrap().0), None);
        fs::remove_dir_all(root).unwrap();
    }

    fn apu(drm: &Path, name: &str, microwatts: &str) {
        card(drm, name, "0x1002", Some(("power1_average", microwatts)));
        fs::write(
            drm.join(name).join("device/hwmon/hwmon3/in1_label"),
            "vddnb\n",
        )
        .unwrap();
    }

    #[test]
    fn an_apu_alone_is_read_as_one() {
        let root = drm("apu");
        apu(&root, "card0", "30000000");
        let (files, apu) = choose(power_files(&root)).unwrap();
        assert!(apu);
        assert_eq!(read_watts(&files), Some(30.0));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_discrete_card_leaves_the_apu_out() {
        let root = drm("mixed");
        apu(&root, "card0", "30000000");
        card(
            &root,
            "card1",
            "0x1002",
            Some(("power1_average", "180000000")),
        );
        let (files, apu) = choose(power_files(&root)).unwrap();
        assert!(!apu);
        assert_eq!(read_watts(&files), Some(180.0));
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
        println!(
            "ADLX reads {}",
            if adlx.apu { "an APU" } else { "discrete GPUs" }
        );
        for _ in 0..10 {
            let watts = adlx.read().expect("a reading");
            println!("ADLX GPU power: {watts:.1} W");
            assert!(watts > 0.0);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
