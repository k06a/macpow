use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::types::BatteryInfo;
use core_foundation_sys::dictionary::CFDictionaryRef;

pub fn read_battery() -> BatteryInfo {
    unsafe { read_battery_inner().unwrap_or_default() }
}

unsafe fn read_battery_inner() -> Option<BatteryInfo> {
    let matching = IOServiceMatching(b"AppleSmartBattery\0".as_ptr() as *const i8);
    if matching.is_null() {
        return None;
    }
    let service = IOServiceGetMatchingService(0, matching);
    if service == 0 {
        return None;
    }

    let mut props = std::ptr::null_mut();
    let kr = IORegistryEntryCreateCFProperties(service, &mut props, std::ptr::null(), 0);
    IOObjectRelease(service);
    if kr != 0 || props.is_null() {
        return None;
    }

    let dict = props as CFDictionaryRef;

    let voltage_mv = cf_utils::cfdict_get_f64(dict, "AppleRawBatteryVoltage")
        .or_else(|| cf_utils::cfdict_get_f64(dict, "Voltage"))
        .unwrap_or(0.0);
    let amperage_ma = cf_utils::cfdict_get_f64(dict, "InstantAmperage")
        .or_else(|| cf_utils::cfdict_get_f64(dict, "Amperage"))
        .unwrap_or(0.0);
    let current_cap = cf_utils::cfdict_get_i64(dict, "CurrentCapacity").unwrap_or(0);
    let max_cap = cf_utils::cfdict_get_i64(dict, "MaxCapacity").unwrap_or(1);
    let os_is_charging = cf_utils::cfdict_get_bool(dict, "IsCharging").unwrap_or(false);
    let external = cf_utils::cfdict_get_bool(dict, "ExternalConnected").unwrap_or(false);
    let raw_time = cf_utils::cfdict_get_i64(dict, "TimeRemaining").unwrap_or(-1);
    let nominal_charge_mah = cf_utils::cfdict_get_f64(dict, "NominalChargeCapacity").unwrap_or(0.0);
    let capacity_wh = nominal_charge_mah * voltage_mv / 1_000_000.0;

    let is_charging = os_is_charging;
    let power_w = voltage_mv * amperage_ma.abs() / 1_000_000.0;
    // drain_w: negative = charging (energy in), positive = discharging (energy out)
    let drain_w = if external { -power_w } else { power_w };
    let percent = if max_cap > 0 {
        (current_cap as f64 / max_cap as f64) * 100.0
    } else {
        0.0
    };

    // Pass raw macOS estimate; metrics thread will compute SMA-based fallback
    let time_remaining = if raw_time > 0 && raw_time < 6000 {
        raw_time
    } else {
        -1
    };

    cf_utils::cf_release(props as _);

    Some(BatteryInfo {
        present: true,
        charging: is_charging,
        voltage_mv,
        amperage_ma,
        drain_w,
        capacity_wh,
        current_capacity: current_cap,
        max_capacity: max_cap,
        percent,
        time_remaining_min: time_remaining,
        external_connected: external,
    })
}
