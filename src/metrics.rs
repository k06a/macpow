use crate::battery;
use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::ioreport::IOReportSampler;
use crate::peripherals;
use crate::powermetrics;
use crate::smc::SmcConnection;
use crate::types::*;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use std::time::{Duration, Instant};

// ── Power estimation constants ───────────────────────────────────────────────

/// Max display power at 100% brightness (built-in Liquid Retina XDR)
pub const MAX_DISPLAY_W: f32 = 5.0;
/// Max keyboard backlight power at 100%
pub const MAX_KEYBOARD_W: f32 = 0.5;
/// Max single-fan power at full RPM (cubic model)
pub const MAX_FAN_W: f32 = 1.0;
/// Audio DAC/amp idle power when device is open
pub const AUDIO_IDLE_W: f32 = 0.05;
/// Max speaker power at full volume
pub const MAX_SPEAKER_W: f32 = 1.0;
/// BT audio device power (headphones, speakers)
pub const BT_AUDIO_DEVICE_W: f32 = 0.05;
/// BT peripheral power (keyboard, mouse, etc.)
pub const BT_PERIPHERAL_W: f32 = 0.01;
/// SSD idle power
pub const SSD_IDLE_W: f32 = 0.03;
/// SSD max active power under heavy I/O
pub const SSD_MAX_ACTIVE_W: f32 = 2.5;

// ── CoreGraphics for display ID ──────────────────────────────────────────────

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// Cached DisplayServices function pointer (loaded once via dlopen).
static DISPLAY_BRIGHTNESS_FN: std::sync::OnceLock<
    Option<unsafe extern "C" fn(u32, *mut f32) -> i32>,
> = std::sync::OnceLock::new();

fn load_display_brightness_fn() -> Option<unsafe extern "C" fn(u32, *mut f32) -> i32> {
    use std::ffi::CStr;
    unsafe {
        let path = CStr::from_bytes_with_nul_unchecked(
            b"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices\0",
        );
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let sym_name = CStr::from_bytes_with_nul_unchecked(b"DisplayServicesGetBrightness\0");
        let sym = libc::dlsym(handle, sym_name.as_ptr());
        if sym.is_null() {
            return None;
        }
        // Don't dlclose — keep the library loaded for the process lifetime
        Some(std::mem::transmute::<
            *mut libc::c_void,
            unsafe extern "C" fn(u32, *mut f32) -> i32,
        >(sym))
    }
}

/// Read current display brightness (0.0–1.0) via DisplayServices private framework.
fn read_display_brightness() -> Option<f32> {
    let get_brightness = (*DISPLAY_BRIGHTNESS_FN.get_or_init(load_display_brightness_fn))?;
    unsafe {
        let display = CGMainDisplayID();
        let mut br: f32 = 0.0;
        let ret = get_brightness(display, &mut br);
        if ret == 0 {
            Some(br.clamp(0.0, 1.0))
        } else {
            None
        }
    }
}

/// Read max display nits from AppleARMBacklight (static, read once).
fn read_display_max_nits() -> f32 {
    unsafe {
        let matching = IOServiceMatching(b"AppleARMBacklight\0".as_ptr() as *const i8);
        if matching.is_null() {
            return 500.0;
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return 500.0;
        }
        let service = IOIteratorNext(iter);
        if service == 0 {
            IOObjectRelease(iter);
            return 500.0;
        }

        let mut props = std::ptr::null_mut();
        let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
        IOObjectRelease(service);
        IOObjectRelease(iter);
        if kr != 0 || props.is_null() {
            return 500.0;
        }

        let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
        let params = cf_utils::cfdict_get(dict, "IODisplayParameters");
        if params.is_null() {
            cf_utils::cf_release(props as _);
            return 500.0;
        }
        let mn = cf_utils::cfdict_get(params as _, "BrightnessMilliNits");
        if mn.is_null() {
            cf_utils::cf_release(props as _);
            return 500.0;
        }
        let max = cf_utils::cfdict_get_i64(mn as _, "max").unwrap_or(500_000);
        cf_utils::cf_release(props as _);

        (max as f32 / 1000.0).max(1.0)
    }
}

/// Keyboard backlight: reverse-map PWM duty cycle through the nits-to-pwm table
/// to recover the actual 0-100% brightness level.
fn read_keyboard_brightness() -> Option<f32> {
    unsafe {
        let matching = IOServiceMatching(b"AppleARMPWMDevice\0".as_ptr() as *const i8);
        if matching.is_null() {
            return None;
        }

        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return None;
        }

        let mut result: Option<f32> = None;

        loop {
            let service = IOIteratorNext(iter);
            if service == 0 {
                break;
            }

            let mut name_buf = [0i8; 128];
            IORegistryEntryGetName(service, name_buf.as_mut_ptr());
            let name = std::ffi::CStr::from_ptr(name_buf.as_ptr()).to_string_lossy();

            if name.contains("kbd-backlight") {
                let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
                if IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0) == 0
                    && !props.is_null()
                {
                    let dict = props as CFDictionaryRef;
                    let high = cf_utils::cfdict_get_i64(dict, "high-period").unwrap_or(0) as f64;
                    let low = cf_utils::cfdict_get_i64(dict, "low-period").unwrap_or(0) as f64;
                    let total = high + low;

                    if total > 0.0 {
                        // Build the PWM lookup table from the nits-to-pwm data
                        let pwm_table = read_pwm_table(dict, total);
                        if !pwm_table.is_empty() {
                            result = Some(reverse_lookup_brightness(&pwm_table, high as f32));
                        } else {
                            // Fallback: raw duty cycle
                            result = Some((high / total) as f32);
                        }
                    }
                    cf_utils::cf_release(props as _);
                }
            }

            IOObjectRelease(service);
            if result.is_some() {
                break;
            }
        }

        IOObjectRelease(iter);
        result
    }
}

/// Read nits-to-pwm-percentage-part1 table, scale entries to high-period space.
/// Part1 covers the keyboard's usable brightness range.
fn read_pwm_table(dict: CFDictionaryRef, total_period: f64) -> Vec<f32> {
    // Use part1 as the primary lookup table — it covers the normal brightness range.
    // Part2 extends to higher nits (different driver mode) and isn't used here.
    let val = unsafe { cf_utils::cfdict_get(dict, "nits-to-pwm-percentage-part1") };
    if val.is_null() {
        return Vec::new();
    }
    let len = unsafe { core_foundation_sys::data::CFDataGetLength(val as _) };
    let ptr = unsafe { core_foundation_sys::data::CFDataGetBytePtr(val as _) };
    if ptr.is_null() || len < 4 {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let raw: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    let max_raw = raw.iter().copied().max().unwrap_or(0);
    if max_raw == 0 {
        return Vec::new();
    }

    // Scale: last entry in part1 = max high-period for keyboard brightness
    let scale = total_period as f32 / max_raw as f32;
    raw.iter().map(|&v| v as f32 * scale).collect()
}

/// Given a sorted table of high-period values (one per brightness step),
/// find where `current_hp` falls and return 0.0–1.0 brightness.
fn reverse_lookup_brightness(table: &[f32], current_hp: f32) -> f32 {
    if table.is_empty() {
        return 0.0;
    }
    if current_hp <= table[0] {
        return 0.0;
    }
    if current_hp >= *table.last().unwrap() {
        return 1.0;
    }
    // Binary search for the insertion point, then interpolate
    let pos = table.partition_point(|&v| v < current_hp);
    if pos == 0 {
        return 0.0;
    }
    let lo = table[pos - 1];
    let hi = table[pos];
    let frac = if hi > lo {
        (current_hp - lo) / (hi - lo)
    } else {
        0.0
    };
    let idx = (pos - 1) as f32 + frac;
    (idx / (table.len() - 1) as f32).clamp(0.0, 1.0)
}

// ── Audio: volume via CoreAudio + playback detection via power assertions ────

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object_id: u32,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const libc::c_void,
        data_size: *mut u32,
        data: *mut libc::c_void,
    ) -> i32;
}

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const AUDIO_OBJECT_SYSTEM: u32 = 1;
const AUDIO_HARDWARE_PROP_DEFAULT_OUTPUT: u32 = u32::from_be_bytes(*b"dOut");
const AUDIO_DEVICE_PROP_VOLUME_SCALAR: u32 = u32::from_be_bytes(*b"volm");
const AUDIO_DEVICE_PROP_MUTE: u32 = u32::from_be_bytes(*b"mute");
const AUDIO_OBJECT_PROP_SCOPE_OUTPUT: u32 = u32::from_be_bytes(*b"outp");
const AUDIO_OBJECT_PROP_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
const AUDIO_OBJECT_PROP_ELEMENT_MAIN: u32 = 0;

fn read_audio_volume() -> (Option<f32>, bool) {
    unsafe {
        let addr = AudioObjectPropertyAddress {
            selector: AUDIO_HARDWARE_PROP_DEFAULT_OUTPUT,
            scope: AUDIO_OBJECT_PROP_SCOPE_GLOBAL,
            element: AUDIO_OBJECT_PROP_ELEMENT_MAIN,
        };
        let mut device_id: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let kr = AudioObjectGetPropertyData(
            AUDIO_OBJECT_SYSTEM,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut device_id as *mut u32 as *mut libc::c_void,
        );
        if kr != 0 || device_id == 0 {
            return (None, false);
        }

        let vol_addr = AudioObjectPropertyAddress {
            selector: AUDIO_DEVICE_PROP_VOLUME_SCALAR,
            scope: AUDIO_OBJECT_PROP_SCOPE_OUTPUT,
            element: AUDIO_OBJECT_PROP_ELEMENT_MAIN,
        };
        let mut volume: f32 = 0.0;
        let mut vol_size = std::mem::size_of::<f32>() as u32;
        let vol_ok = AudioObjectGetPropertyData(
            device_id,
            &vol_addr,
            0,
            std::ptr::null(),
            &mut vol_size,
            &mut volume as *mut f32 as *mut libc::c_void,
        ) == 0;

        let mute_addr = AudioObjectPropertyAddress {
            selector: AUDIO_DEVICE_PROP_MUTE,
            scope: AUDIO_OBJECT_PROP_SCOPE_OUTPUT,
            element: AUDIO_OBJECT_PROP_ELEMENT_MAIN,
        };
        let mut muted: u32 = 0;
        let mut mute_size = std::mem::size_of::<u32>() as u32;
        let _ = AudioObjectGetPropertyData(
            device_id,
            &mute_addr,
            0,
            std::ptr::null(),
            &mut mute_size,
            &mut muted as *mut u32 as *mut libc::c_void,
        );

        let vol_pct = if vol_ok {
            Some((volume * 100.0).clamp(0.0, 100.0))
        } else {
            None
        };
        (vol_pct, muted != 0)
    }
}

fn detect_audio_from_assertions(assertions: &[crate::types::PowerAssertion]) -> (bool, bool) {
    let device_active = assertions
        .iter()
        .any(|a| a.name.contains("BuiltInSpeakerDevice") || a.name.contains("audio-out"));
    let playing = assertions.iter().any(|a| a.name.contains("AudioTap"));
    (device_active, playing)
}

fn read_audio_info(assertions: &[crate::types::PowerAssertion]) -> AudioInfo {
    let (volume, muted) = read_audio_volume();
    let (device_active, playing) = detect_audio_from_assertions(assertions);

    let effective_vol = if muted {
        0.0
    } else {
        volume.unwrap_or(0.0) / 100.0
    };
    let estimated_power_w = match (device_active, playing) {
        (false, _) => 0.0,
        (true, true) => AUDIO_IDLE_W + effective_vol * effective_vol * MAX_SPEAKER_W,
        (true, false) => AUDIO_IDLE_W,
    };

    AudioInfo {
        volume_pct: volume,
        muted,
        device_active,
        playing,
        estimated_power_w,
    }
}

// ── GPU core count from IORegistry ───────────────────────────────────────────

fn read_gpu_core_count() -> u32 {
    std::process::Command::new("sysctl")
        .args(["-n", "machdep.gpu.core_count"])
        .output().ok()
        .and_then(|o| std::str::from_utf8(&o.stdout).ok()?.trim().parse().ok())
        .or_else(|| {
            std::process::Command::new("bash")
                .args(["-c", r#"ioreg -l 2>/dev/null | grep -oE '"gpu-core-count" = [0-9]+' | head -1 | grep -oE '[0-9]+$'"#])
                .output().ok()
                .and_then(|o| std::str::from_utf8(&o.stdout).ok()?.trim().parse().ok())
        })
        .unwrap_or(0)
}

// ── Sampler ──────────────────────────────────────────────────────────────────

// ── Per-process energy via proc_pid_rusage ───────────────────────────────────

extern "C" {
    fn proc_listallpids(buffer: *mut i32, buffersize: i32) -> i32;
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u8) -> i32;
    fn proc_name(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
}

const RUSAGE_INFO_V4: i32 = 4;
// ri_billed_energy is at byte offset 256 in rusage_info_v4 (field index 32, u64)
const BILLED_ENERGY_OFFSET: usize = 32 * 8;
const RUSAGE_V4_SIZE: usize = 36 * 8; // 36 u64 fields

fn read_all_process_energy() -> std::collections::HashMap<i32, (String, u64)> {
    let mut result = std::collections::HashMap::new();
    unsafe {
        let mut pids = vec![0i32; 4096];
        let n = proc_listallpids(pids.as_mut_ptr(), (pids.len() * 4) as i32);
        if n <= 0 {
            return result;
        }

        let mut buf = vec![0u8; RUSAGE_V4_SIZE];
        let mut name_buf = vec![0u8; 256];

        for i in 0..n as usize {
            let pid = pids[i];
            if pid <= 0 {
                continue;
            }

            let ret = proc_pid_rusage(pid, RUSAGE_INFO_V4, buf.as_mut_ptr());
            if ret != 0 {
                continue;
            }

            let energy_nj = u64::from_ne_bytes(
                buf[BILLED_ENERGY_OFFSET..BILLED_ENERGY_OFFSET + 8]
                    .try_into()
                    .unwrap_or([0; 8]),
            );
            if energy_nj == 0 {
                continue;
            }

            name_buf.fill(0);
            proc_name(pid, name_buf.as_mut_ptr(), 256);
            let name = std::ffi::CStr::from_ptr(name_buf.as_ptr() as *const i8)
                .to_string_lossy()
                .into_owned();

            result.insert(pid, (name, energy_nj));
        }
    }
    result
}

fn read_dram_gb() -> u32 {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| {
            std::str::from_utf8(&o.stdout)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|bytes| (bytes / (1024 * 1024 * 1024)) as u32)
        .unwrap_or(0)
}

fn read_ssd_model() -> String {
    unsafe {
        let matching = IOServiceMatching(b"IONVMeController\0".as_ptr() as *const i8);
        if matching.is_null() {
            return String::new();
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return String::new();
        }
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            IOObjectRelease(iter);
            return String::new();
        }
        let mut props = std::ptr::null_mut();
        let mut model = String::new();
        let mut interconnect = String::new();
        if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
            model = cf_utils::cfdict_get_string(dict, "Model Number")
                .unwrap_or_default()
                .trim()
                .to_string();
            interconnect =
                cf_utils::cfdict_get_string(dict, "Physical Interconnect").unwrap_or_default();
            cf_utils::cf_release(props as _);
        }
        IOObjectRelease(entry);
        IOObjectRelease(iter);
        if !model.is_empty() {
            format!("{}, {}", model, interconnect)
        } else {
            "NVMe".into()
        }
    }
}

extern "C" {
    fn host_statistics64(host: u32, flavor: i32, info: *mut u8, count: *mut u32) -> i32;
}
const HOST_VM_INFO64: i32 = 4;
const HOST_VM_INFO64_COUNT: u32 = 38; // sizeof(vm_statistics64_data_t) / sizeof(integer_t)
const PAGE_SIZE: u64 = 16384;

fn read_mem_used_gb() -> f32 {
    #[repr(C)]
    struct VmStats64 {
        free_count: u32,
        active_count: u32,
        inactive_count: u32,
        wire_count: u32,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: u32,
        speculative_count: u32,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: u32,
        throttled_count: u32,
        external_page_count: u32,
        internal_page_count: u32,
        total_uncompressed_pages_in_compressor: u64,
    }
    unsafe {
        let mut info = std::mem::zeroed::<VmStats64>();
        let mut count = HOST_VM_INFO64_COUNT;
        let kr = host_statistics64(
            mach_host_self(),
            HOST_VM_INFO64,
            &mut info as *mut VmStats64 as *mut u8,
            &mut count,
        );
        if kr != 0 {
            return 0.0;
        }
        let used_pages = info.active_count as u64
            + info.inactive_count as u64
            + info.wire_count as u64
            + info.compressor_page_count as u64;
        (used_pages * PAGE_SIZE) as f32 / (1024.0 * 1024.0 * 1024.0)
    }
}

fn read_gpu_utilization() -> (u32, u32, u32) {
    static GPU_CLASS: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();

    let cached = GPU_CLASS.get_or_init(|| {
        for gen in (13..=19).rev() {
            for suffix in &["X\0", "G\0", "P\0"] {
                let name = format!("AGXAcceleratorG{}{}", gen, suffix);
                let matched = unsafe {
                    let m = IOServiceMatching(name.as_ptr() as *const i8);
                    if m.is_null() {
                        false
                    } else {
                        let s = IOServiceGetMatchingService(0, m);
                        if s != 0 {
                            IOObjectRelease(s);
                            true
                        } else {
                            false
                        }
                    }
                };
                if matched {
                    return Some(name.into_bytes());
                }
            }
        }
        None
    });

    match cached {
        Some(name) => unsafe { try_gpu_util_class(name) },
        None => (0, 0, 0),
    }
}

unsafe fn try_gpu_util_class(class_name: &[u8]) -> (u32, u32, u32) {
    let matching = IOServiceMatching(class_name.as_ptr() as *const i8);
    if matching.is_null() {
        return (0, 0, 0);
    }
    let service = IOServiceGetMatchingService(0, matching);
    if service == 0 {
        return (0, 0, 0);
    }
    let mut props = std::ptr::null_mut();
    let result = if IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0) == 0
        && !props.is_null()
    {
        let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
        let stats = cf_utils::cfdict_get(dict, "PerformanceStatistics");
        if !stats.is_null() {
            let sd = stats as core_foundation_sys::dictionary::CFDictionaryRef;
            let dev = cf_utils::cfdict_get_i64(sd, "Device Utilization %").unwrap_or(0) as u32;
            let ren = cf_utils::cfdict_get_i64(sd, "Renderer Utilization %").unwrap_or(0) as u32;
            let til = cf_utils::cfdict_get_i64(sd, "Tiler Utilization %").unwrap_or(0) as u32;
            (dev, ren, til)
        } else {
            (0, 0, 0)
        }
    } else {
        (0, 0, 0)
    };
    if !props.is_null() {
        cf_utils::cf_release(props as _);
    }
    IOObjectRelease(service);
    result
}

// ── Per-CPU utilization via Mach host_processor_info ─────────────────────────

extern "C" {
    fn host_processor_info(
        host: u32,
        flavor: i32,
        out_cpu_count: *mut u32,
        out_info: *mut *mut i32,
        out_count: *mut u32,
    ) -> i32;
    fn mach_host_self() -> u32;
    fn vm_deallocate(target: u32, address: usize, size: usize) -> i32;
}

const PROCESSOR_CPU_LOAD_INFO: i32 = 2;
const CPU_STATE_USER: usize = 0;
const CPU_STATE_SYSTEM: usize = 1;
const CPU_STATE_IDLE: usize = 2;
// const CPU_STATE_NICE: usize = 3;
const CPU_LOAD_FIELDS: usize = 4;

fn read_cpu_ticks() -> Vec<(u64, u64)> {
    unsafe {
        let mut ncpu: u32 = 0;
        let mut info: *mut i32 = std::ptr::null_mut();
        let mut count: u32 = 0;
        if host_processor_info(
            mach_host_self(),
            PROCESSOR_CPU_LOAD_INFO,
            &mut ncpu,
            &mut info,
            &mut count,
        ) != 0
        {
            return Vec::new();
        }
        let result: Vec<(u64, u64)> = (0..ncpu as usize)
            .map(|i| {
                let base = i * CPU_LOAD_FIELDS;
                let user = *info.add(base + CPU_STATE_USER) as u64;
                let sys = *info.add(base + CPU_STATE_SYSTEM) as u64;
                let idle = *info.add(base + CPU_STATE_IDLE) as u64;
                let nice = *info.add(base + 3) as u64;
                let total = user + sys + idle + nice;
                let used = user + sys;
                (used, total)
            })
            .collect();
        vm_deallocate(
            crate::iokit_ffi::mach_task_self(),
            info as usize,
            count as usize * 4,
        );
        result
    }
}

fn compute_cpu_usage(prev: &[(u64, u64)], cur: &[(u64, u64)]) -> Vec<f32> {
    cur.iter()
        .zip(prev.iter())
        .map(|((cu, ct), (pu, pt))| {
            let dt = ct.saturating_sub(*pt);
            let du = cu.saturating_sub(*pu);
            if dt > 0 {
                (du as f32 / dt as f32 * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            }
        })
        .collect()
}

use std::sync::{Arc, Mutex};

pub struct Sampler {
    shared: Arc<Mutex<Metrics>>,
    gpu_cores: u32,
    dram_gb: u32,
    ssd_model: String,
    _handles: Vec<std::thread::JoinHandle<()>>,
}

impl Sampler {
    pub fn new(interval_ms: u64) -> Self {
        // Parallelize init reads — all independent
        let (gpu_cores, dram_gb, ssd_model, max_nits) = std::thread::scope(|s| {
            let h1 = s.spawn(|| std::panic::catch_unwind(read_gpu_core_count).unwrap_or(0));
            let h2 = s.spawn(|| std::panic::catch_unwind(read_dram_gb).unwrap_or(0));
            let h3 = s.spawn(|| std::panic::catch_unwind(read_ssd_model).unwrap_or_default());
            let h4 = s.spawn(|| std::panic::catch_unwind(read_display_max_nits).unwrap_or(500.0));
            (
                h1.join().unwrap_or(0),
                h2.join().unwrap_or(0),
                h3.join().unwrap_or_default(),
                h4.join().unwrap_or(500.0),
            )
        });

        let shared = Arc::new(Mutex::new(Metrics {
            gpu_cores,
            dram_gb,
            ..Default::default()
        }));
        let mut handles = Vec::new();
        let dt = Duration::from_millis(interval_ms.max(100));

        // ── IOReport (SoC power + frequencies) ───────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                let Some(ior) = IOReportSampler::new().ok() else {
                    return;
                };
                let mut prev = ior.sample().ok();
                loop {
                    std::thread::sleep(dt);
                    if let Ok(cur) = ior.sample() {
                        if let Some(ref p) = prev {
                            if let Ok(soc) = ior.parse_power(p, &cur) {
                                if let Ok(mut mg) = m.lock() {
                                    mg.soc = soc;
                                }
                            }
                        }
                        prev = Some(cur);
                    }
                }
            }));
        }

        // ── SMC (temps, fans, system power) ──────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                let Some(mut smc) = SmcConnection::open().ok() else {
                    return;
                };
                let mut disc_handle: Option<std::thread::JoinHandle<Vec<(String, String)>>> =
                    Some(smc.start_temp_discovery());
                let mut prev_ticks = read_cpu_ticks();
                loop {
                    // Non-blocking: check if temp discovery finished
                    if let Some(ref h) = disc_handle {
                        if h.is_finished() {
                            if let Some(h) = disc_handle.take() {
                                smc.finish_temp_discovery(h);
                            }
                        }
                    }
                    let temps = smc.read_temperatures();
                    let fans = smc.read_fans();
                    let sys_power = smc.read_system_power();
                    let cur_ticks = read_cpu_ticks();
                    let cpu_usage = compute_cpu_usage(&prev_ticks, &cur_ticks);
                    prev_ticks = cur_ticks;
                    if let Ok(mut mg) = m.lock() {
                        mg.temperatures = temps;
                        mg.fans = fans;
                        mg.sys_power_w = if sys_power > mg.soc.total_w {
                            sys_power
                        } else {
                            mg.soc.total_w
                        };
                        mg.mem_used_gb = read_mem_used_gb();
                        mg.cpu_usage_pct = cpu_usage;
                        let (_, gr, gt) = read_gpu_utilization();
                        mg.soc.gpu_util_renderer = gr;
                        mg.soc.gpu_util_tiler = gt;
                    }
                    std::thread::sleep(dt);
                }
            }));
        }

        // ── Battery ──────────────────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                let mut b = battery::read_battery();
                if let Ok(mg) = m.lock() {
                    if b.present
                        && !b.external_connected
                        && b.amperage_ma < 0.0
                        && mg.sys_power_w > 0.0
                    {
                        b.drain_w = mg.sys_power_w as f64;
                    }
                }
                if let Ok(mut mg) = m.lock() {
                    mg.battery = b;
                }
                std::thread::sleep(dt);
            }));
        }

        // ── Display brightness ───────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                if let Ok(mut mg) = m.lock() {
                    if let Some(brightness) = read_display_brightness() {
                        mg.display.brightness_pct = brightness * 100.0;
                        mg.display.estimated_power_w = brightness * MAX_DISPLAY_W;
                        mg.display.nits = brightness * max_nits;
                        mg.display.available = true;
                    } else {
                        mg.display.brightness_pct = 0.0;
                        mg.display.estimated_power_w = 0.0;
                        mg.display.nits = 0.0;
                        mg.display.available = false;
                    }
                }
                std::thread::sleep(dt);
            }));
        }

        // ── Keyboard backlight ───────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                if let Some(kbd_level) = read_keyboard_brightness() {
                    if let Ok(mut mg) = m.lock() {
                        mg.keyboard.brightness_pct = kbd_level * 100.0;
                        mg.keyboard.estimated_power_w = kbd_level * MAX_KEYBOARD_W;
                    }
                }
                std::thread::sleep(dt);
            }));
        }

        // ── Audio ────────────────────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                let assertions = m
                    .lock()
                    .ok()
                    .map(|mg| mg.power_assertions.clone())
                    .unwrap_or_default();
                let audio = read_audio_info(&assertions);
                if let Ok(mut mg) = m.lock() {
                    mg.audio = audio;
                }
                std::thread::sleep(dt);
            }));
        }

        // ── Network counters ─────────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                let mut prev: Option<(Instant, powermetrics::NetCounters)> = None;
                loop {
                    let cur = powermetrics::read_net_counters();
                    if let Some((prev_time, ref prev_counters)) = prev {
                        let dt_s = prev_time.elapsed().as_secs_f64();
                        let net = powermetrics::compute_net_rates(prev_counters, &cur, dt_s);
                        if let Ok(mut mg) = m.lock() {
                            mg.network = net;
                        }
                    }
                    prev = Some((Instant::now(), cur));
                    std::thread::sleep(dt);
                }
            }));
        }

        // ── USB + Power assertions ───────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                let usb = peripherals::list_usb_devices();
                let asserts = peripherals::list_power_assertions();
                if let Ok(mut mg) = m.lock() {
                    mg.usb_devices = usb;
                    mg.power_assertions = asserts;
                }
                std::thread::sleep(dt);
            }));
        }

        // ── Bluetooth ────────────────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                let devs = peripherals::read_bluetooth_devices();
                let pw: f32 = devs
                    .iter()
                    .map(|d| {
                        if d.minor_type.contains("Headphone") || d.minor_type.contains("Audio") {
                            BT_AUDIO_DEVICE_W
                        } else {
                            BT_PERIPHERAL_W
                        }
                    })
                    .sum();
                if let Ok(mut mg) = m.lock() {
                    mg.bluetooth_devices = devs;
                    mg.bluetooth_power_w = pw;
                }
                std::thread::sleep(Duration::from_secs(30));
            }));
        }

        // ── WiFi (slow — 10s interval) ──────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || loop {
                let wifi = peripherals::read_wifi_info();
                if let Ok(mut mg) = m.lock() {
                    mg.wifi = wifi;
                }
                std::thread::sleep(Duration::from_secs(10));
            }));
        }

        // ── Disk I/O → SSD power estimation (IORegistry counters, no subprocess)
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                let mut prev = powermetrics::read_disk_counters();
                let mut prev_time = Instant::now();
                loop {
                    std::thread::sleep(dt);
                    let cur = powermetrics::read_disk_counters();
                    let now = Instant::now();
                    let dt_s = now.duration_since(prev_time).as_secs_f64();
                    let disk = powermetrics::compute_disk_rates(&prev, &cur, dt_s);
                    let total_bps = disk.read_bytes_per_sec + disk.write_bytes_per_sec;
                    let max_bps = 3_000.0 * 1024.0 * 1024.0;
                    let utilization = (total_bps / max_bps).clamp(0.0, 1.0) as f32;
                    let power = SSD_IDLE_W + utilization * (SSD_MAX_ACTIVE_W - SSD_IDLE_W);
                    if let Ok(mut mg) = m.lock() {
                        mg.disk = disk;
                        mg.ssd_power_w = power;
                    }
                    prev = cur;
                    prev_time = now;
                }
            }));
        }

        // ── Per-process energy ───────────────────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                // (name, last_energy_nj, session_energy_mj, last_delta_nj, last_dt_s, last_seen)
                let mut known: std::collections::HashMap<
                    i32,
                    (String, u64, f64, u64, f64, Instant),
                > = std::collections::HashMap::new();
                loop {
                    let cur = read_all_process_energy();
                    let now = Instant::now();

                    for (&pid, (name, energy_nj)) in &cur {
                        let energy_nj = *energy_nj;
                        let entry = known
                            .entry(pid)
                            .or_insert_with(|| (name.clone(), energy_nj, 0.0, 0, 0.0, now));
                        let delta_nj = energy_nj.saturating_sub(entry.1);
                        let dt_s = now.duration_since(entry.5).as_secs_f64();
                        entry.0 = name.clone();
                        entry.1 = energy_nj;
                        entry.2 += delta_nj as f64 / 1e6;
                        entry.3 = delta_nj;
                        entry.4 = dt_s;
                        entry.5 = now;
                    }

                    known.retain(|_, (_, _, _, _, _, seen)| {
                        now.duration_since(*seen).as_secs() < 30
                    });

                    let mut procs: Vec<ProcessPower> = known
                        .iter()
                        .filter(|(_, (_, _, mj, _, _, _))| *mj > 0.0)
                        .map(|(&pid, (name, _, session_mj, delta_nj, dt_s, _))| {
                            let power_w = if *dt_s > 0.01 {
                                (*delta_nj as f64 / 1e9 / dt_s) as f32
                            } else {
                                0.0
                            };
                            ProcessPower {
                                pid,
                                name: name.clone(),
                                power_w,
                                energy_mj: *session_mj,
                            }
                        })
                        .collect();
                    let total_power: f32 = procs.iter().map(|p| p.power_w).sum();
                    let total_energy: f64 = procs.iter().map(|p| p.energy_mj).sum();
                    procs.sort_by(|a, b| {
                        b.energy_mj
                            .partial_cmp(&a.energy_mj)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    procs.truncate(50);
                    if let Ok(mut mg) = m.lock() {
                        mg.all_procs_power_w = total_power;
                        mg.all_procs_energy_mj = total_energy;
                        mg.top_processes = procs;
                    }

                    std::thread::sleep(dt);
                }
            }));
        }

        Self {
            shared,
            gpu_cores,
            dram_gb,
            ssd_model,
            _handles: handles,
        }
    }

    /// Return a snapshot of the current metrics (non-blocking).
    pub fn snapshot(&self) -> Metrics {
        let mut m = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        m.gpu_cores = self.gpu_cores;
        m.dram_gb = self.dram_gb;
        m.ssd_model = self.ssd_model.clone();
        m
    }
}
