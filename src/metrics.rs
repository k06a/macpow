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

/// Read current display brightness (0.0–1.0) via DisplayServices private framework.
/// Loaded dynamically since it's not a public framework.
fn read_display_brightness() -> Option<f32> {
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
            libc::dlclose(handle);
            return None;
        }
        let get_brightness: unsafe extern "C" fn(u32, *mut f32) -> i32 = std::mem::transmute(sym);

        let display = CGMainDisplayID();
        let mut br: f32 = 0.0;
        let ret = get_brightness(display, &mut br);
        libc::dlclose(handle);

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

// ── Audio: volume via osascript + playback detection via pmset assertions ────

fn read_audio_info() -> AudioInfo {
    let output = match std::process::Command::new("osascript")
        .args(["-e", "get volume settings"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return AudioInfo::default(),
    };

    let volume = output.split(',').find_map(|p| {
        p.trim()
            .strip_prefix("output volume:")?
            .trim()
            .parse::<f32>()
            .ok()
    });
    let muted = output.split(',').any(|p| {
        p.trim()
            .strip_prefix("output muted:")
            .map(|v| v.trim() == "true")
            .unwrap_or(false)
    });

    let (device_active, playing) = detect_audio_playback();

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

/// Check `pmset -g assertions` for audio activity:
/// - `BuiltInSpeakerDevice` assertion → audio device is open
/// - `AudioTap` assertion → audio is actively playing/routing
fn detect_audio_playback() -> (bool, bool) {
    let output = match std::process::Command::new("pmset")
        .args(["-g", "assertions"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return (false, false),
    };

    let device_active = output
        .lines()
        .any(|l| l.contains("BuiltInSpeakerDevice") || l.contains("audio-out"));
    let playing = output.lines().any(|l| l.contains("AudioTap"));
    (device_active, playing)
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

use std::sync::{Arc, Mutex};

pub struct Sampler {
    shared: Arc<Mutex<Metrics>>,
    gpu_cores: u32,
    dram_gb: u32,
    _handles: Vec<std::thread::JoinHandle<()>>,
}

impl Sampler {
    pub fn new(interval_ms: u64) -> Self {
        let gpu_cores = read_gpu_core_count();
        let dram_gb = read_dram_gb();
        let max_nits = read_display_max_nits();

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
                let handle = smc.start_temp_discovery();
                smc.finish_temp_discovery(handle);
                loop {
                    let temps = smc.read_temperatures();
                    let fans = smc.read_fans();
                    let sys_power = smc.read_system_power();
                    if let Ok(mut mg) = m.lock() {
                        mg.temperatures = temps;
                        mg.fans = fans;
                        mg.sys_power_w = if sys_power > mg.soc.total_w {
                            sys_power
                        } else {
                            mg.soc.total_w
                        };
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
                if let Some(brightness) = read_display_brightness() {
                    if let Ok(mut mg) = m.lock() {
                        mg.display.brightness_pct = brightness * 100.0;
                        mg.display.estimated_power_w = brightness * MAX_DISPLAY_W;
                        mg.display.nits = brightness * max_nits;
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
                let audio = read_audio_info();
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
                std::thread::sleep(dt);
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

        // ── Disk I/O → SSD power estimation ─────────────────────────────
        {
            let m = shared.clone();
            handles.push(std::thread::spawn(move || {
                loop {
                    let disk = powermetrics::read_disk_rates();
                    let total_bps = disk.read_bytes_per_sec + disk.write_bytes_per_sec;
                    let max_bps = 3_000.0 * 1024.0 * 1024.0; // ~3 GB/s for Apple NVMe
                    let utilization = (total_bps / max_bps).clamp(0.0, 1.0) as f32;
                    let power = SSD_IDLE_W + utilization * (SSD_MAX_ACTIVE_W - SSD_IDLE_W);
                    if let Ok(mut mg) = m.lock() {
                        mg.disk = disk;
                        mg.ssd_power_w = power;
                    }
                    std::thread::sleep(dt);
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
        m
    }
}
