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
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGDisplayScreenSize(display: u32) -> CGSize;
    fn CGDisplayCopyDisplayMode(display: u32) -> CGDisplayModeRef;
    fn CGDisplayCopyAllDisplayModes(
        display: u32,
        options: core_foundation_sys::dictionary::CFDictionaryRef,
    ) -> core_foundation_sys::array::CFArrayRef;
    fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    fn CGDisplayModeRelease(mode: CGDisplayModeRef);
}

#[repr(C)]
struct CGDisplayMode {
    _private: [u8; 0],
}
type CGDisplayModeRef = *mut CGDisplayMode;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

/// Cached DisplayServices function pointers (loaded once via dlopen).
static DISPLAY_BRIGHTNESS_FN: std::sync::OnceLock<
    Option<unsafe extern "C" fn(u32, *mut f32) -> i32>,
> = std::sync::OnceLock::new();
static DISPLAY_LINEAR_BRIGHTNESS_FN: std::sync::OnceLock<
    Option<unsafe extern "C" fn(u32, *mut f32) -> i32>,
> = std::sync::OnceLock::new();

/// CoreDisplay private API for reading the active Reference Mode (Display Preset).
/// These are not in any public header but exported as plain C from
/// `/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay`.
type CoreDisplayGetActivePresetIndex = unsafe extern "C" fn(u32) -> i32;
type CoreDisplayCopyPreset = unsafe extern "C" fn(u32, i32) -> CFDictionaryRef;
static CD_GET_ACTIVE_PRESET_FN: std::sync::OnceLock<Option<CoreDisplayGetActivePresetIndex>> =
    std::sync::OnceLock::new();
static CD_COPY_PRESET_FN: std::sync::OnceLock<Option<CoreDisplayCopyPreset>> =
    std::sync::OnceLock::new();

fn load_core_display_fn<T: Copy>(sym_name: &[u8]) -> Option<T> {
    use std::ffi::CStr;
    unsafe {
        let path = CStr::from_bytes_with_nul_unchecked(
            b"/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay\0",
        );
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let sym_cstr = CStr::from_bytes_with_nul_unchecked(sym_name);
        let sym = libc::dlsym(handle, sym_cstr.as_ptr());
        if sym.is_null() {
            return None;
        }
        // Don't dlclose — keep the library loaded for the process lifetime
        Some(*(&sym as *const *mut libc::c_void as *const T))
    }
}

fn load_display_brightness_fn() -> Option<unsafe extern "C" fn(u32, *mut f32) -> i32> {
    load_display_services_fn(b"DisplayServicesGetBrightness\0")
}

fn load_display_linear_brightness_fn() -> Option<unsafe extern "C" fn(u32, *mut f32) -> i32> {
    load_display_services_fn(b"DisplayServicesGetLinearBrightness\0")
}

fn load_display_services_fn(sym_name: &[u8]) -> Option<unsafe extern "C" fn(u32, *mut f32) -> i32> {
    use std::ffi::CStr;
    unsafe {
        let path = CStr::from_bytes_with_nul_unchecked(
            b"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices\0",
        );
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let sym_cstr = CStr::from_bytes_with_nul_unchecked(sym_name);
        let sym = libc::dlsym(handle, sym_cstr.as_ptr());
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
pub fn read_display_brightness() -> Option<f32> {
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

/// Read linear brightness (0.0–1.0) — proportional to actual light output (nits).
/// Unlike GetBrightness (perceptual/gamma-corrected), this is linear:
/// nits ≈ linear_brightness * max_nits_hdr.
pub fn read_display_linear_brightness() -> Option<f32> {
    let get_linear =
        (*DISPLAY_LINEAR_BRIGHTNESS_FN.get_or_init(load_display_linear_brightness_fn))?;
    unsafe {
        let display = CGMainDisplayID();
        let mut lbr: f32 = 0.0;
        let ret = get_linear(display, &mut lbr);
        if ret == 0 {
            Some(lbr.max(0.0))
        } else {
            None
        }
    }
}

/// Classify the panel from its peak millinits into static SDR/HDR/XDR class.
/// Thresholds: ≤500 nits → SDR, 501–999 → HDR, ≥1000 → XDR (Apple flagship mini-LED).
pub fn classify_panel(max_nits: f32) -> PanelClass {
    if max_nits >= 1000.0 {
        PanelClass::Xdr
    } else if max_nits > 500.0 {
        PanelClass::Hdr
    } else {
        PanelClass::Sdr
    }
}

/// Current refresh rate of the main display in Hz.
/// Returns 0.0 if unavailable. Used to tag the display label as 60Hz / 120Hz
/// or "ProMotion" when the panel supports variable refresh.
pub fn read_display_refresh_hz() -> f32 {
    unsafe {
        let display = CGMainDisplayID();
        let mode = CGDisplayCopyDisplayMode(display);
        if mode.is_null() {
            return 0.0;
        }
        let hz = CGDisplayModeGetRefreshRate(mode) as f32;
        CGDisplayModeRelease(mode);
        hz
    }
}

/// Detect ProMotion support by checking if any available display mode reports a
/// refresh rate above 119 Hz (M1 Pro/Max and newer XDR notebooks). Cached at
/// startup since panel hardware doesn't change at runtime.
pub fn read_supports_promotion() -> bool {
    unsafe {
        let display = CGMainDisplayID();
        let arr = CGDisplayCopyAllDisplayModes(display, std::ptr::null());
        if arr.is_null() {
            return false;
        }
        let count = core_foundation_sys::array::CFArrayGetCount(arr);
        let mut found = false;
        for i in 0..count {
            let mode =
                core_foundation_sys::array::CFArrayGetValueAtIndex(arr, i) as CGDisplayModeRef;
            if !mode.is_null() && CGDisplayModeGetRefreshRate(mode) >= 119.0 {
                found = true;
                break;
            }
        }
        cf_utils::cf_release(arr as _);
        found
    }
}

/// Information about the active CoreDisplay Reference Mode (a.k.a. "Display
/// Preset" in System Settings → Displays). This is the **dynamic** display mode
/// the user has chosen, e.g. "Apple Display (P3-600 nits)", "HDR Video
/// (P3-ST 2084)", "Photography (P3-D65)". Values reflect the current preset and
/// update without process restart when the user switches modes.
#[derive(Clone, Debug, Default)]
pub struct DisplayPreset {
    pub name: String,
    pub max_sdr_nits: f32,
    pub max_hdr_nits: f32,
    pub max_edr_headroom: f32,
}

/// Query the active Reference Mode via CoreDisplay private API.
/// Returns `None` if the framework or symbols can't be loaded (highly unlikely
/// on Apple Silicon Macs, but kept defensive).
pub fn read_display_preset() -> Option<DisplayPreset> {
    let get_idx = (*CD_GET_ACTIVE_PRESET_FN
        .get_or_init(|| load_core_display_fn(b"CoreDisplay_Display_GetActivePresetIndex\0")))?;
    let copy_preset = (*CD_COPY_PRESET_FN
        .get_or_init(|| load_core_display_fn(b"CoreDisplay_Display_CopyPreset\0")))?;
    unsafe {
        let display = CGMainDisplayID();
        let idx = get_idx(display);
        if idx < 0 {
            return None;
        }
        let dict = copy_preset(display, idx);
        if dict.is_null() {
            return None;
        }
        let name = cf_utils::cfdict_get_string(dict, "PresetName").unwrap_or_default();
        let max_sdr_nits =
            cf_utils::cfdict_get_f64(dict, "PresetMaxSDRLuminance").unwrap_or(0.0) as f32;
        let max_hdr_nits =
            cf_utils::cfdict_get_f64(dict, "PresetMaxHDRLuminance").unwrap_or(0.0) as f32;
        let max_edr_headroom = cf_utils::cfdict_get_f64(dict, "PresetHostMaxPotentialEDRHeadroom")
            .unwrap_or(0.0) as f32;
        cf_utils::cf_release(dict as _);
        Some(DisplayPreset {
            name,
            max_sdr_nits,
            max_hdr_nits,
            max_edr_headroom,
        })
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

/// Assumed backlight LED string voltage (not exposed by macOS).
const BACKLIGHT_VOLTAGE_V: f32 = 6.0;

/// Read backlight calibration: max current in mA (static, read once).
fn read_backlight_max_current_ma() -> f32 {
    unsafe {
        let matching = IOServiceMatching(b"AppleARMBacklight\0".as_ptr() as *const i8);
        if matching.is_null() {
            return 0.0;
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return 0.0;
        }
        let service = IOIteratorNext(iter);
        if service == 0 {
            IOObjectRelease(iter);
            return 0.0;
        }

        let mut props = std::ptr::null_mut();
        let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
        IOObjectRelease(service);
        IOObjectRelease(iter);
        if kr != 0 || props.is_null() {
            return 0.0;
        }

        let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
        let cal = cf_utils::cfdict_get(dict, "backlight-calibration-parameters");
        let max_ua = if !cal.is_null() {
            cf_utils::cfdict_get_i64(cal as _, "current-for-max-backlight").unwrap_or(0)
        } else {
            0
        };
        cf_utils::cf_release(props as _);

        max_ua as f32 / 1000.0
    }
}

/// Read real brightness and nits from AppleARMBacklight IODisplayParameters.
/// Returns (brightness_value, brightness_max, millinits_value, millinits_max).
/// More accurate than DisplayServicesGetBrightness: shows real nits after
/// auto-brightness and Apple's SDR/HDR software clamp.
pub fn read_backlight_brightness() -> Option<(i64, i64, i64, i64)> {
    unsafe {
        let matching = IOServiceMatching(b"AppleARMBacklight\0".as_ptr() as *const i8);
        if matching.is_null() {
            return None;
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return None;
        }
        let service = IOIteratorNext(iter);
        if service == 0 {
            IOObjectRelease(iter);
            return None;
        }
        let mut props = std::ptr::null_mut();
        let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
        IOObjectRelease(service);
        IOObjectRelease(iter);
        if kr != 0 || props.is_null() {
            return None;
        }
        let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
        let params = cf_utils::cfdict_get(dict, "IODisplayParameters");
        if params.is_null() {
            cf_utils::cf_release(props as _);
            return None;
        }
        let br = cf_utils::cfdict_get(params as _, "brightness");
        let mn = cf_utils::cfdict_get(params as _, "BrightnessMilliNits");
        let result = if !br.is_null() && !mn.is_null() {
            let br_val = cf_utils::cfdict_get_i64(br as _, "value").unwrap_or(0);
            let br_max = cf_utils::cfdict_get_i64(br as _, "max").unwrap_or(1);
            let mn_val = cf_utils::cfdict_get_i64(mn as _, "value").unwrap_or(0);
            let mn_max = cf_utils::cfdict_get_i64(mn as _, "max").unwrap_or(1);
            Some((br_val, br_max, mn_val, mn_max))
        } else {
            None
        };
        cf_utils::cf_release(props as _);
        result
    }
}

/// Read current BrightnessMicroAmps from AppleARMBacklight IODisplayParameters.
/// Returns (current_ua, max_ua) or None if unavailable.
pub fn read_backlight_current() -> Option<(i64, i64)> {
    unsafe {
        let matching = IOServiceMatching(b"AppleARMBacklight\0".as_ptr() as *const i8);
        if matching.is_null() {
            return None;
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return None;
        }
        let service = IOIteratorNext(iter);
        if service == 0 {
            IOObjectRelease(iter);
            return None;
        }

        let mut props = std::ptr::null_mut();
        let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
        IOObjectRelease(service);
        IOObjectRelease(iter);
        if kr != 0 || props.is_null() {
            return None;
        }

        let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
        let params = cf_utils::cfdict_get(dict, "IODisplayParameters");
        if params.is_null() {
            cf_utils::cf_release(props as _);
            return None;
        }
        let ua = cf_utils::cfdict_get(params as _, "BrightnessMicroAmps");
        if ua.is_null() {
            cf_utils::cf_release(props as _);
            return None;
        }
        let cur = cf_utils::cfdict_get_i64(ua as _, "value").unwrap_or(0);
        let max = cf_utils::cfdict_get_i64(ua as _, "max").unwrap_or(0);
        cf_utils::cf_release(props as _);

        if max > 0 {
            Some((cur, max))
        } else {
            None
        }
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
            name_buf[127] = 0;
            if IORegistryEntryGetName(service, name_buf.as_mut_ptr()) != 0 {
                IOObjectRelease(service);
                continue;
            }
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
        .chunks(4)
        .filter(|c| c.len() == 4)
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

pub fn read_audio_volume() -> (Option<f32>, bool) {
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

/// GPU core count: the `gpu-core-count` property of the AGX accelerator, the
/// registry service that `read_gpu_utilization` polls. 0 when there is none.
fn read_gpu_core_count() -> u32 {
    // SAFETY: IOKit calls on objects owned by this function. The matching
    // dictionary is consumed by IOServiceGetMatchingServices; the iterator,
    // every service it yields and every property dictionary are released
    // exactly once, and `props` is alive while `gpu_core_count_prop` reads it.
    unsafe {
        // Every AGXAcceleratorG<generation><variant> driver class derives from
        // IOAccelerator, so the base class finds the GPU on any chip.
        let matching = IOServiceMatching(b"IOAccelerator\0".as_ptr() as *const i8);
        if matching.is_null() {
            return 0;
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return 0;
        }
        let mut cores = None;
        while cores.is_none() {
            let service = IOIteratorNext(iter);
            if service == 0 {
                break;
            }
            let mut props = std::ptr::null_mut();
            if IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0) == 0
                && !props.is_null()
            {
                cores = gpu_core_count_prop(props as CFDictionaryRef);
                cf_utils::cf_release(props as _);
            }
            IOObjectRelease(service);
        }
        IOObjectRelease(iter);
        cores.unwrap_or(0)
    }
}

/// `gpu-core-count` from the properties of one accelerator service.
unsafe fn gpu_core_count_prop(props: CFDictionaryRef) -> Option<u32> {
    let value = cf_utils::cfdict_get(props, "gpu-core-count");
    // The value is whatever the driver published, and reading anything but a
    // CFNumber as a number is undefined behaviour, so check the type first.
    if value.is_null()
        || core_foundation_sys::base::CFGetTypeID(value)
            != core_foundation_sys::number::CFNumberGetTypeID()
    {
        return None;
    }
    cf_utils::cfdict_get_i64(props, "gpu-core-count").and_then(|n| u32::try_from(n).ok())
}

#[cfg(test)]
mod gpu_core_count_tests {
    use super::gpu_core_count_prop;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    fn prop_of(pairs: &[(&str, CFType)]) -> Option<u32> {
        let pairs: Vec<(CFString, CFType)> = pairs
            .iter()
            .map(|(key, value)| (CFString::new(key), value.clone()))
            .collect();
        let dict = CFDictionary::from_CFType_pairs(&pairs);
        // SAFETY: `dict` is a valid CFDictionary that outlives the call.
        unsafe { gpu_core_count_prop(dict.as_concrete_TypeRef()) }
    }

    #[test]
    fn reads_a_number_property() {
        let cores = CFNumber::from(40i64).as_CFType();
        let other = CFNumber::from(7i64).as_CFType();
        assert_eq!(
            prop_of(&[("gpu-core-count", cores), ("other", other)]),
            Some(40)
        );
    }

    #[test]
    fn ignores_a_missing_non_number_or_negative_property() {
        let other = CFNumber::from(7i64).as_CFType();
        assert_eq!(prop_of(&[("other", other)]), None);
        let text = CFString::new("40").as_CFType();
        assert_eq!(prop_of(&[("gpu-core-count", text)]), None);
        let negative = CFNumber::from(-1i64).as_CFType();
        assert_eq!(prop_of(&[("gpu-core-count", negative)]), None);
    }
}

// ── Sampler ──────────────────────────────────────────────────────────────────

// ── Per-process energy via proc_pid_rusage ───────────────────────────────────

// The reader and the accounting live in `process_power`. Re-exported so that
// `metrics::read_all_process_energy` keeps working for existing callers.
pub use crate::process_power::read_all_process_energy;

fn read_dram_gb() -> u32 {
    crate::topology::sysctl_u64("hw.memsize")
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
    fn host_page_size(host: u32, out_page_size: *mut usize) -> i32;
}
const HOST_VM_INFO64: i32 = 4;
const HOST_VM_INFO64_COUNT: u32 = 38; // HOST_VM_INFO64_REV1_COUNT: VmStats64 in integer_t units

/// `vm_statistics64` from <mach/vm_statistics.h> up to its revision 1, which
/// is all the kernel fills in for HOST_VM_INFO64_COUNT. The `*_count` fields
/// are numbers of pages.
#[repr(C)]
#[derive(Default)]
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

// host_statistics64 writes HOST_VM_INFO64_COUNT integers into a VmStats64.
const _: () = assert!(
    std::mem::size_of::<VmStats64>() == HOST_VM_INFO64_COUNT as usize * std::mem::size_of::<i32>()
);

/// Pages behind Activity Monitor's "Memory Used": app memory (anonymous pages
/// that are not purgeable) + wired + compressor. File cache and purgeable
/// pages are left out because the kernel gives them up on demand; counting
/// them (active + inactive) made the figure creep up to the whole RAM on any
/// Mac with some uptime.
fn mem_used_pages(vm: &VmStats64) -> u64 {
    let app = (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64);
    app + vm.wire_count as u64 + vm.compressor_page_count as u64
}

pub fn read_mem_used_gb() -> f32 {
    let mut vm = VmStats64::default();
    let mut count = HOST_VM_INFO64_COUNT;
    // The kernel counts in its own page size (16 KiB on Apple Silicon, 4 KiB
    // on Intel), which host_page_size reports.
    let mut page_size: usize = 0;
    // SAFETY: `vm` is exactly `count` integers long (asserted above), so the
    // kernel cannot write past it, and any bit pattern is a valid VmStats64.
    // `page_size` is a valid out-pointer for the vm_size_t the call returns.
    let ok = unsafe {
        let host = mach_host_self();
        host_statistics64(
            host,
            HOST_VM_INFO64,
            &mut vm as *mut VmStats64 as *mut u8,
            &mut count,
        ) == 0
            && host_page_size(host, &mut page_size) == 0
    };
    if !ok {
        return 0.0;
    }
    (mem_used_pages(&vm) * page_size as u64) as f32 / (1024.0 * 1024.0 * 1024.0)
}

/// (device, renderer, tiler) utilisation in percent as the GPU driver reports
/// them; all zero when there is no such driver (virtual machines).
pub fn read_gpu_utilization() -> (u32, u32, u32) {
    // Every Apple GPU driver (AGXAcceleratorG13G ... AGXAcceleratorG17X) is a
    // subclass of IOAccelerator, so matching the base class also finds the
    // generations that do not exist yet.
    // SAFETY: the class name is NUL-terminated, the only thing
    // try_gpu_util_class requires of its argument.
    unsafe { try_gpu_util_class(b"IOAccelerator\0") }
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
        // Any IOAccelerator can match, so do not take it on trust that the
        // property is the dictionary the Apple GPU drivers publish.
        if !stats.is_null()
            && core_foundation_sys::base::CFGetTypeID(stats)
                == core_foundation_sys::dictionary::CFDictionaryGetTypeID()
        {
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

/// IOReport GPU residency (percent) from which all zeros from the driver
/// cannot be a GPU that had nothing to do.
const GPU_BUSY_MIN_PCT: u32 = 5;
/// Collided readings in a row that keep the last good values. After that the
/// values would be too old to pass for a measurement and the zeros go out.
const GPU_MAX_BRIDGED_READS: u32 = 5;

/// Tells driver readings that lost a race from real ones.
///
/// The GPU driver refreshes its utilisation figures only every 130-270 ms
/// (measured on an M5 Max) and a read clears them until the next refresh.
/// Whoever reads second in between gets 0/0/0: another macpow, Activity
/// Monitor, ioreg, or this process itself at a short sampling interval.
/// IOReport's GPU residency is not affected, so all zeros while IOReport
/// shows a busy GPU mean "no data", not "idle". Reading again would not
/// help: it takes the next refresh away from the other reader.
#[derive(Default)]
struct GpuCollisionFilter {
    bridged_reads: u32,
}

impl GpuCollisionFilter {
    /// The (renderer, tiler) pair to publish for a driver reading, or `None`
    /// when the reading collided and the last published pair should stay.
    fn accept(
        &mut self,
        (device, renderer, tiler): (u32, u32, u32),
        ioreport_device_pct: u32,
    ) -> Option<(u32, u32)> {
        let collided =
            (device, renderer, tiler) == (0, 0, 0) && ioreport_device_pct >= GPU_BUSY_MIN_PCT;
        if !collided {
            self.bridged_reads = 0;
        } else if self.bridged_reads < GPU_MAX_BRIDGED_READS {
            self.bridged_reads += 1;
            return None;
        }
        Some((renderer, tiler))
    }
}

// ── System stats: CPU usage, memory, GPU utilisation ─────────────────────────

/// One reading of the metrics that come from Mach and the IORegistry alone.
/// They need neither the SMC nor IOReport, so they also work in a virtual
/// machine or a sandbox where those are missing.
struct SystemStats {
    /// One value per logical CPU. Empty on a reader's first reading, which
    /// has no earlier tick counts to measure an interval against.
    cpu_usage_pct: Vec<f32>,
    mem_used_gb: f32,
    /// (device, renderer, tiler) as the GPU driver reported them.
    gpu_util: (u32, u32, u32),
}

#[derive(Default)]
struct SystemStatsReader {
    prev_cpu_ticks: Vec<(u64, u64)>,
}

impl SystemStatsReader {
    fn read(&mut self) -> SystemStats {
        let cpu_ticks = read_cpu_ticks();
        let cpu_usage_pct = compute_cpu_usage(&self.prev_cpu_ticks, &cpu_ticks);
        self.prev_cpu_ticks = cpu_ticks;
        SystemStats {
            cpu_usage_pct,
            mem_used_gb: read_mem_used_gb(),
            gpu_util: read_gpu_utilization(),
        }
    }
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

pub fn read_cpu_ticks() -> Vec<(u64, u64)> {
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
        if info.is_null() || ncpu == 0 {
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct StaticSnapshot {
    gpu_cores: u32,
    dram_gb: u32,
    ssd_model: String,
    cpu_tier_names: Vec<String>,
    cpu_high_tier: Vec<bool>,
}

fn spawn_periodic(
    handles: &mut Vec<std::thread::JoinHandle<()>>,
    running: &Arc<AtomicBool>,
    period: Duration,
    mut tick: impl FnMut() + Send + 'static,
) {
    let running = running.clone();
    handles.push(std::thread::spawn(move || {
        // Why this helper exists:
        // it enforces one shutdown policy for all sources, so we don't end up
        // with subtly different loop/exit behaviors across threads.
        while running.load(Ordering::Relaxed) {
            tick();
            std::thread::sleep(period);
        }
    }));
}

pub struct Sampler {
    shared: Arc<Mutex<Metrics>>,
    static_snapshot: StaticSnapshot,
    running: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
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

        // Pre-seed the active preset so the first display tick has a sane cap
        // for the live-nits formula instead of falling back to panel_max_nits.
        let initial_preset = read_display_preset();
        let mut initial_metrics = Metrics {
            gpu_cores,
            dram_gb,
            ..Default::default()
        };
        if let Some(p) = initial_preset {
            initial_metrics.display.preset_name = p.name;
            initial_metrics.display.preset_max_sdr_nits = p.max_sdr_nits;
            initial_metrics.display.preset_max_hdr_nits = p.max_hdr_nits;
            initial_metrics.display.preset_max_edr_headroom = p.max_edr_headroom;
        }
        let shared = Arc::new(Mutex::new(initial_metrics));
        let static_snapshot = StaticSnapshot {
            gpu_cores,
            dram_gb,
            ssd_model,
            cpu_tier_names: crate::topology::tier_names().to_vec(),
            cpu_high_tier: crate::topology::CpuTopology::read().high_tier_mask(),
        };
        let running = Arc::new(AtomicBool::new(true));
        let mut handles = Vec::new();
        // Used exactly as given. The CLI validates `--interval` once (100 ms
        // to 60 s); a second, silent floor here made the two disagree.
        let dt = Duration::from_millis(interval_ms);

        // ── IOReport (SoC power + frequencies) ───────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                let Ok(ior) = IOReportSampler::new() else {
                    // No IOReport at all (any virtual machine, a sandbox):
                    // nothing will ever fill `soc`. Flag it, or its zeros
                    // read as an idle chip.
                    if let Ok(mut mg) = m.lock() {
                        crate::ioreport::mark_unavailable(&mut mg.soc);
                    }
                    return;
                };
                let mut prev = ior.sample().ok();
                // Remembers across ticks whether the Energy Model is gated
                // (macOS 27+) and which live source replaces it.
                let mut estimator = ior.estimator();
                if let Some(ref p) = prev {
                    estimator.seed_core_shares(ior.core_share_prior(p));
                }
                while running.load(Ordering::Relaxed) {
                    std::thread::sleep(dt);
                    if let Ok(cur) = ior.sample() {
                        let backlight = ior.parse_backlight(&cur);
                        if let Some(ref p) = prev {
                            if let Ok(raw) = ior.parse_raw(p, &cur) {
                                let mut soc = estimator.assemble(raw);
                                if let Ok(mut mg) = m.lock() {
                                    // The SMC thread owns these two fields.
                                    soc.gpu_util_renderer = mg.soc.gpu_util_renderer;
                                    soc.gpu_util_tiler = mg.soc.gpu_util_tiler;
                                    mg.soc = soc;
                                    let dpb = backlight.dpb_factor();
                                    mg.display.dpb_factor = dpb;
                                    // Treat anything materially above 1.0 as
                                    // "HDR engaged"; the 1.05 threshold filters
                                    // tiny fixed-point rounding noise around 1.0.
                                    mg.display.hdr_active = dpb > 1.05;
                                    // NOTE: AppleARMBacklight `MilliNits value`
                                    // is published but doesn't update with the
                                    // slider on Apple Silicon (it only refreshes
                                    // on certain backlight events), so we don't
                                    // use it for live nits. The display thread
                                    // computes nits from the slider × preset cap.
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
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                let Some(mut smc) = SmcConnection::open().ok() else {
                    return;
                };
                let mut disc_handle: Option<std::thread::JoinHandle<Vec<(String, String)>>> =
                    Some(smc.start_temp_discovery());
                while running.load(Ordering::Relaxed) {
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
                    // PBwo is the new backlight rail on M5 Pro/Max/Neo (and A18);
                    // PDBR ("Power Delivery Brightness") is the older XDR rail on M1-M4.
                    // Try the new key first; on hardware where it doesn't exist
                    // the call errors and we fall back transparently.
                    let backlight_power = smc
                        .read_f32("PBwo")
                        .or_else(|_| smc.read_f32("PDBR"))
                        .unwrap_or(0.0);
                    let adapter_power = smc.read_f32("PDTR").unwrap_or(0.0);
                    let wifi_power = smc.read_f32("wiPm").unwrap_or(0.0);
                    let usb_power_smc = smc.read_f32("PUSB").unwrap_or(0.0);
                    if let Ok(mut mg) = m.lock() {
                        mg.temperatures = temps;
                        mg.fans = fans;
                        mg.sys_power_w = if sys_power > mg.soc.total_w {
                            sys_power
                        } else {
                            mg.soc.total_w
                        };
                        mg.backlight_power_w = backlight_power;
                        mg.adapter_power_w = adapter_power;
                        mg.wifi_power_w = wifi_power;
                        mg.usb_power_smc_w = usb_power_smc;
                    }
                    std::thread::sleep(dt);
                }
                if let Some(h) = disc_handle.take() {
                    if h.is_finished() {
                        smc.finish_temp_discovery(h);
                    }
                }
            }));
        }

        // ── System stats (CPU usage, memory, GPU utilisation) ────────────
        // Kept apart from the SMC source: that one ends at once where the SMC
        // cannot be opened, and none of these values depends on it.
        {
            let m = shared.clone();
            let mut reader = SystemStatsReader::default();
            let mut gpu_filter = GpuCollisionFilter::default();
            spawn_periodic(&mut handles, &running, dt, move || {
                // Every system call is made here, before the lock is taken.
                let stats = reader.read();
                if let Ok(mut mg) = m.lock() {
                    mg.cpu_usage_pct = stats.cpu_usage_pct;
                    mg.mem_used_gb = stats.mem_used_gb;
                    if let Some((renderer, tiler)) =
                        gpu_filter.accept(stats.gpu_util, mg.soc.gpu_util_device)
                    {
                        mg.soc.gpu_util_renderer = renderer;
                        mg.soc.gpu_util_tiler = tiler;
                    }
                }
            });
        }

        // ── Battery ──────────────────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                let mut power_sma = crate::sma::TimeSma::new(300.0); // 5-minute window
                while running.load(Ordering::Relaxed) {
                    let mut b = battery::read_battery();
                    if let Ok(mg) = m.lock() {
                        if b.present && !b.external_connected && mg.sys_power_w > 0.0 {
                            b.drain_w = mg.sys_power_w as f64;
                        }
                    }
                    // Feed absolute power into 5-min SMA for time estimation
                    if b.present {
                        power_sma.push(b.drain_w.abs() as f32);
                    }
                    // Compute time remaining from SMA when macOS doesn't provide it
                    if b.present && b.time_remaining_min <= 0 && b.capacity_wh > 0.0 {
                        let avg_power = power_sma.get() as f64;
                        if avg_power > 0.5 {
                            let remaining_wh = b.capacity_wh * b.percent / 100.0;
                            b.time_remaining_min = if b.external_connected {
                                let full_wh = b.capacity_wh - remaining_wh;
                                (full_wh / avg_power * 60.0) as i64
                            } else {
                                (remaining_wh / avg_power * 60.0) as i64
                            };
                        }
                    }
                    if let Ok(mut mg) = m.lock() {
                        mg.battery = b;
                        mg.adapter = battery::read_adapter();
                    }
                    std::thread::sleep(dt);
                }
            }));
        }

        // ── Display brightness + class/refresh/ProMotion ─────────────────
        // Static panel class is computed once from peak millinits. Refresh rate
        // and ProMotion support are read from CGDisplay APIs (in-process, no
        // subprocess). Together with `hdr_active` (live from IOReport DPB factor
        // in the IOReport thread above) this fully replaces the old Python EDR
        // subprocess that caused issue #9 Dock flashing.
        {
            let m = shared.clone();
            let cal_max_ma = read_backlight_max_current_ma();
            let panel_class = classify_panel(max_nits);
            let supports_promotion = read_supports_promotion();
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    let brightness = read_display_brightness();
                    let backlight_current = read_backlight_current();
                    let refresh_hz = read_display_refresh_hz();
                    let preset = read_display_preset();
                    if let Ok(mut mg) = m.lock() {
                        if let Some(br) = brightness {
                            mg.display.available = true;
                            mg.display.brightness_pct = br * 100.0;
                            mg.display.max_nits = max_nits;
                            // Map the slider linearly to the active Reference
                            // Mode's full peak (e.g. 1600 in "P3-1600",
                            // 600 in "P3-600"). This matches the user's
                            // mental model: slider 100% = mode's stated peak,
                            // exactly the number shown in System Settings.
                            //
                            // We deliberately don't try to reflect the
                            // SDR-vs-HDR-boost split: physically, an SDR
                            // pixel at slider 100% in "P3-1600" tops out near
                            // 600 nits and only HDR content can push to 1600
                            // via DPB boost — but that's confusing UX. Apple's
                            // own Settings UI labels the peak as the slider
                            // ceiling, so we follow that convention.
                            //
                            // Limitation: AppleARMBacklight's `MilliNits` /
                            // `MicroAmps` channels don't refresh on slider
                            // movement on Apple Silicon (event-driven, often
                            // stale for many seconds), so we can't use the
                            // actual measured brightness for live display.
                            // Thermal throttling on XDR mini-LED can also
                            // reduce achievable nits ~10–15% under sustained
                            // peak HDR, but we have no clean live signal for
                            // that.
                            let sdr_cap = if mg.display.preset_max_sdr_nits > 0.0 {
                                mg.display.preset_max_sdr_nits
                            } else {
                                max_nits
                            };
                            let hdr_cap = if mg.display.preset_max_hdr_nits > 0.0 {
                                mg.display.preset_max_hdr_nits.max(sdr_cap)
                            } else {
                                sdr_cap
                            };
                            mg.display.peak_nits = hdr_cap;
                            mg.display.nits = br * hdr_cap;
                            mg.display.panel_class = panel_class;
                            mg.display.refresh_hz = refresh_hz;
                            mg.display.supports_promotion = supports_promotion;
                            if let Some(p) = preset {
                                mg.display.preset_name = p.name;
                                mg.display.preset_max_sdr_nits = p.max_sdr_nits;
                                mg.display.preset_max_hdr_nits = p.max_hdr_nits;
                                mg.display.preset_max_edr_headroom = p.max_edr_headroom;
                            }
                            // Power from backlight current or linear estimate
                            mg.display.estimated_power_w =
                                if let Some((cur_ua, max_ua)) = backlight_current {
                                    if max_ua > 0 {
                                        let frac = cur_ua as f32 / max_ua as f32;
                                        let real_ma = frac * cal_max_ma;
                                        real_ma / 1000.0 * BACKLIGHT_VOLTAGE_V
                                    } else {
                                        br * MAX_DISPLAY_W
                                    }
                                } else {
                                    br * MAX_DISPLAY_W
                                };
                            let display = unsafe { CGMainDisplayID() };
                            mg.display.width_px = unsafe { CGDisplayPixelsWide(display) } as u32;
                            mg.display.height_px = unsafe { CGDisplayPixelsHigh(display) } as u32;
                            let size_mm = unsafe { CGDisplayScreenSize(display) };
                            let diag_mm = (size_mm.width.powi(2) + size_mm.height.powi(2)).sqrt();
                            mg.display.diagonal_inches =
                                (diag_mm / 25.4 * 10.0).round() as f32 / 10.0;
                        } else {
                            mg.display.brightness_pct = 0.0;
                            mg.display.estimated_power_w = 0.0;
                            mg.display.nits = 0.0;
                            mg.display.available = false;
                        }
                    }
                    std::thread::sleep(dt);
                }
            }));
        }

        // ── Keyboard backlight ───────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            spawn_periodic(&mut handles, &running, dt, move || {
                if let Some(kbd_level) = read_keyboard_brightness() {
                    if let Ok(mut mg) = m.lock() {
                        mg.keyboard.brightness_pct = kbd_level * 100.0;
                        mg.keyboard.estimated_power_w = kbd_level * MAX_KEYBOARD_W;
                    }
                }
            });
        }

        // ── Audio ────────────────────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            spawn_periodic(&mut handles, &running, dt, move || {
                let assertions = m
                    .lock()
                    .ok()
                    .map(|mg| mg.power_assertions.clone())
                    .unwrap_or_default();
                let audio = read_audio_info(&assertions);
                if let Ok(mut mg) = m.lock() {
                    mg.audio = audio;
                }
            });
        }

        // ── Network counters ─────────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                let mut prev: Option<(Instant, powermetrics::NetCounters)> = None;
                while running.load(Ordering::Relaxed) {
                    let cur = powermetrics::read_net_counters();
                    if let Some((prev_time, ref prev_counters)) = prev {
                        let dt_s = prev_time.elapsed().as_secs_f64();
                        let net = powermetrics::compute_net_rates(prev_counters, &cur, dt_s);
                        // Read interface names from shared state (set by WiFi+Ethernet thread)
                        let (eth_iface, wifi_iface) = m
                            .lock()
                            .map(|mg| {
                                (
                                    mg.ethernet.interface_name.clone(),
                                    mg.wifi.interface_name.clone(),
                                )
                            })
                            .unwrap_or_default();
                        let eth_net = if !eth_iface.is_empty() {
                            powermetrics::compute_net_rates_iface(
                                prev_counters,
                                &cur,
                                dt_s,
                                &eth_iface,
                            )
                        } else {
                            Default::default()
                        };
                        let wifi_net = if !wifi_iface.is_empty() {
                            powermetrics::compute_net_rates_iface(
                                prev_counters,
                                &cur,
                                dt_s,
                                &wifi_iface,
                            )
                        } else {
                            Default::default()
                        };
                        if let Ok(mut mg) = m.lock() {
                            mg.network = net;
                            mg.eth_network = eth_net;
                            mg.wifi_network = wifi_net;
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
            let running = running.clone();
            // Per-port last measured watts (+ timestamp) for hysteresis.
            let mut last_port_w: std::collections::HashMap<u32, (f32, Instant)> =
                std::collections::HashMap::new();
            const PORT_W_GRACE: Duration = Duration::from_secs(8);
            spawn_periodic(&mut handles, &running, dt, move || {
                let usb = peripherals::list_usb_devices();
                let asserts = peripherals::list_power_assertions();
                let mut usb_ports = battery::read_usb_power_out_per_port();
                // Bridge transient PowerOutDetails gaps: hold the last measured
                // watts for a short grace window so a still-sourcing port doesn't
                // flicker to 0 W while USB-PD renegotiates. Presence itself is
                // governed by the sourcing flag, so an unplugged port (dropped
                // from PortControllerInfo) disappears immediately regardless.
                let now = Instant::now();
                let live: std::collections::HashSet<u32> =
                    usb_ports.iter().map(|p| p.port_index).collect();
                last_port_w.retain(|k, _| live.contains(k));
                for p in usb_ports.iter_mut() {
                    if p.power_w > 0.0 {
                        last_port_w.insert(p.port_index, (p.power_w, now));
                    } else if let Some(&(w, t)) = last_port_w.get(&p.port_index) {
                        if now.duration_since(t) <= PORT_W_GRACE {
                            p.power_w = w;
                        }
                    }
                }
                if let Ok(mut mg) = m.lock() {
                    mg.usb_devices = usb;
                    mg.power_assertions = asserts;
                    // Not `.sum()`: a float sum over no ports is -0.0, which
                    // JSON prints as "-0.0".
                    mg.usb_power_out_w = usb_ports.iter().fold(0.0, |w, p| w + p.power_w);
                    mg.usb_power_per_port = usb_ports;
                }
            });
        }

        // ── Bluetooth ────────────────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            // Read every BLUETOOTH_POLL_PERIOD, not every tick. The thread keeps
            // ticking at `dt` and skips in between: spawn_periodic sleeps a whole
            // period at a time and Drop joins the thread, so passing the 10 s
            // period here would make dropping a Sampler take up to 10 s.
            let mut next_poll = Instant::now();
            spawn_periodic(&mut handles, &running, dt, move || {
                if !peripherals::bluetooth_poll_due(&mut next_poll, Instant::now()) {
                    return;
                }
                let devs = peripherals::read_bluetooth_devices();
                let pw: f32 = devs.len() as f32 * BT_PERIPHERAL_W;
                if let Ok(mut mg) = m.lock() {
                    mg.bluetooth_devices = devs;
                    mg.bluetooth_power_w = pw;
                }
            });
        }

        // ── WiFi + Ethernet ──────────────────────────────────────────────
        {
            let m = shared.clone();
            let running = running.clone();
            let mut wifi_reader = peripherals::WifiReader::default();
            spawn_periodic(&mut handles, &running, dt, move || {
                let wifi = wifi_reader.read();
                let ethernet = peripherals::read_ethernet_info(&wifi.interface_name);
                if let Ok(mut mg) = m.lock() {
                    mg.wifi = wifi;
                    mg.ethernet = ethernet;
                }
            });
        }

        // ── Disk I/O → SSD power estimation (IORegistry counters, no subprocess)
        {
            let m = shared.clone();
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                let mut prev = powermetrics::read_disk_counters();
                let mut prev_time = Instant::now();
                while running.load(Ordering::Relaxed) {
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
            let running = running.clone();
            handles.push(std::thread::spawn(move || {
                use crate::process_power::{refresh_due, ProcessTracker, NET_COUNTERS_REFRESH};
                let mut tracker = ProcessTracker::default();
                let mut net = powermetrics::ProcNetCounters::new();
                let mut net_read_at = None;
                while running.load(Ordering::Relaxed) {
                    let scan = read_all_process_energy();
                    // Stamp the scan before anything slower runs, so the time
                    // between two stamps is the time the energy deltas cover.
                    let now = Instant::now();
                    if refresh_due(net_read_at, now, NET_COUNTERS_REFRESH) {
                        net = powermetrics::read_proc_net_counters();
                        net_read_at = Some(now);
                    }

                    let mut report = tracker.update(&scan, now);
                    for p in &mut report.processes {
                        if let Some(&(rx, tx)) = net.get(&p.pid) {
                            p.net_rx_bytes = rx;
                            p.net_tx_bytes = tx;
                        }
                    }
                    if let Ok(mut mg) = m.lock() {
                        mg.all_procs_power_w = report.total_power_w;
                        mg.all_procs_energy_mj = report.total_energy_mj;
                        mg.top_processes = report.processes;
                    }

                    std::thread::sleep(dt);
                }
            }));
        }

        Self {
            shared,
            static_snapshot,
            running,
            handles,
        }
    }

    /// Return a snapshot of the current metrics (non-blocking).
    pub fn snapshot(&self) -> Metrics {
        let mut m = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Why keep static fields outside shared mutex:
        // they never change after startup, so reading them here avoids extra
        // mutable traffic through the shared state that all workers contend on.
        m.gpu_cores = self.static_snapshot.gpu_cores;
        m.dram_gb = self.static_snapshot.dram_gb;
        m.ssd_model = self.static_snapshot.ssd_model.clone();
        m.cpu_tier_names = self.static_snapshot.cpu_tier_names.clone();
        m.cpu_high_tier = self.static_snapshot.cpu_high_tier.clone();
        m
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_filter_publishes_real_readings() {
        let mut filter = GpuCollisionFilter::default();
        assert_eq!(filter.accept((24, 17, 10), 22), Some((17, 10)));
        // Busy device with an idle renderer and tiler is a reading too.
        assert_eq!(filter.accept((30, 0, 0), 30), Some((0, 0)));
    }

    #[test]
    fn gpu_filter_keeps_last_values_when_a_read_collides() {
        let mut filter = GpuCollisionFilter::default();
        assert_eq!(filter.accept((0, 0, 0), 22), None);
        assert_eq!(filter.accept((0, 0, 0), GPU_BUSY_MIN_PCT), None);
    }

    #[test]
    fn gpu_filter_publishes_zeros_of_an_idle_gpu() {
        let mut filter = GpuCollisionFilter::default();
        assert_eq!(filter.accept((0, 0, 0), 0), Some((0, 0)));
        assert_eq!(filter.accept((0, 0, 0), GPU_BUSY_MIN_PCT - 1), Some((0, 0)));
    }

    #[test]
    fn gpu_filter_gives_up_on_a_long_run_of_collisions() {
        let mut filter = GpuCollisionFilter::default();
        for _ in 0..GPU_MAX_BRIDGED_READS {
            assert_eq!(filter.accept((0, 0, 0), 40), None);
        }
        // The kept values are too old by now: say "no data" instead.
        assert_eq!(filter.accept((0, 0, 0), 40), Some((0, 0)));
        assert_eq!(filter.accept((0, 0, 0), 40), Some((0, 0)));
        // A real reading starts a fresh run.
        assert_eq!(filter.accept((41, 35, 6), 40), Some((35, 6)));
        assert_eq!(filter.accept((0, 0, 0), 40), None);
    }

    // Counters of an M5 Max with 128 GB as vm_stat printed them, in pages of
    // 16 KiB: Activity Monitor showed 82.3 GiB used while active + inactive +
    // wired + compressor came to 126.3 GiB.
    fn vm_stats_with_file_cache() -> VmStats64 {
        VmStats64 {
            active_count: 3_112_965,
            inactive_count: 3_158_230,
            wire_count: 552_682,
            compressor_page_count: 1_456_189,
            internal_page_count: 3_655_584,
            external_page_count: 2_615_920,
            purgeable_count: 272_722,
            ..Default::default()
        }
    }

    #[test]
    fn mem_used_pages_is_app_memory_plus_wired_plus_compressor() {
        let vm = vm_stats_with_file_cache();
        assert_eq!(
            mem_used_pages(&vm),
            3_655_584 - 272_722 + 552_682 + 1_456_189
        );
        assert_eq!(mem_used_pages(&vm) * 16384 / (1 << 30), 82);
    }

    #[test]
    fn mem_used_pages_does_not_count_file_cache() {
        let mut vm = vm_stats_with_file_cache();
        let used = mem_used_pages(&vm);
        // A large file is read: its pages are file-backed and sit on the
        // active and inactive queues until the kernel needs them back.
        vm.external_page_count += 1_000_000;
        vm.active_count += 400_000;
        vm.inactive_count += 600_000;
        assert_eq!(mem_used_pages(&vm), used);
    }

    #[test]
    fn mem_used_pages_survives_more_purgeable_than_anonymous_pages() {
        let vm = VmStats64 {
            internal_page_count: 10,
            purgeable_count: 25,
            wire_count: 3,
            compressor_page_count: 4,
            ..Default::default()
        };
        assert_eq!(mem_used_pages(&vm), 7);
    }

    // Needs neither the SMC nor IOReport, so it has to pass in a VM too.
    #[test]
    fn system_stats_reader_covers_every_logical_cpu_and_used_memory() {
        // SAFETY: sysconf takes no pointers and has no preconditions.
        let logical_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        assert!(logical_cpus > 0);

        let mut reader = SystemStatsReader::default();
        let first = reader.read();
        assert!(
            first.cpu_usage_pct.is_empty(),
            "usage needs two tick readings, got {:?}",
            first.cpu_usage_pct
        );
        assert!(first.mem_used_gb > 0.0, "used memory {}", first.mem_used_gb);

        let second = reader.read();
        assert_eq!(second.cpu_usage_pct.len(), logical_cpus as usize);
        assert!(second
            .cpu_usage_pct
            .iter()
            .all(|pct| (0.0..=100.0).contains(pct)));
        assert!(second.mem_used_gb > 0.0);
    }

    #[test]
    fn compute_cpu_usage_returns_expected_percentages() {
        let prev = vec![(100, 200), (80, 200)];
        let cur = vec![(150, 300), (100, 200)];
        let usage = compute_cpu_usage(&prev, &cur);
        assert_eq!(usage.len(), 2);
        assert!((usage[0] - 50.0).abs() < 1e-6);
        assert_eq!(usage[1], 0.0);
    }

    #[test]
    fn compute_cpu_usage_handles_counter_reset() {
        let prev = vec![(1000, 2000)];
        let cur = vec![(10, 20)];
        let usage = compute_cpu_usage(&prev, &cur);
        assert_eq!(usage, vec![0.0]);
    }
}
