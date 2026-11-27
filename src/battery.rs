use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::types::{AdapterInfo, BatteryInfo, UsbPortPower};
use core_foundation_sys::dictionary::CFDictionaryRef;

pub fn read_battery() -> BatteryInfo {
    unsafe { read_battery_inner().unwrap_or_default() }
}

pub fn read_adapter() -> AdapterInfo {
    unsafe { read_adapter_inner().unwrap_or_default() }
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

    // Temperature is in hundredths of a degree Celsius (e.g. 3045 = 30.45°C)
    let temp_raw = cf_utils::cfdict_get_f64(dict, "Temperature").unwrap_or(0.0);
    let temperature_c = temp_raw / 100.0;

    let cycle_count = cf_utils::cfdict_get_i64(dict, "CycleCount").unwrap_or(0);
    let design_capacity_mah = cf_utils::cfdict_get_f64(dict, "DesignCapacity").unwrap_or(0.0);
    let max_capacity_mah =
        cf_utils::cfdict_get_f64(dict, "AppleRawMaxCapacity").unwrap_or(design_capacity_mah);

    let health_pct = if design_capacity_mah > 0.0 {
        if let Some(battery_data) = cf_utils::cfdict_get_dict(dict, "BatteryData") {
            let fcc_comp1 = cf_utils::cfdict_get_f64(battery_data, "FccComp1").unwrap_or(0.0);
            if fcc_comp1 > 0.0 {
                fcc_comp1 / design_capacity_mah * 100.0
            } else {
                max_capacity_mah / design_capacity_mah * 100.0
            }
        } else {
            max_capacity_mah / design_capacity_mah * 100.0
        }
    } else {
        100.0
    };

    let is_charging = os_is_charging;
    let power_w = voltage_mv * amperage_ma.abs() / 1_000_000.0;
    let drain_w = if external { -power_w } else { power_w };
    let percent = if max_cap > 0 {
        (current_cap as f64 / max_cap as f64) * 100.0
    } else {
        0.0
    };

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
        temperature_c,
        cycle_count,
        design_capacity_mah,
        max_capacity_mah,
        health_pct,
    })
}

unsafe fn read_adapter_inner() -> Option<AdapterInfo> {
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
    let external = cf_utils::cfdict_get_bool(dict, "ExternalConnected").unwrap_or(false);

    if !external {
        cf_utils::cf_release(props as _);
        return Some(AdapterInfo::default());
    }

    let adapter = cf_utils::cfdict_get_dict(dict, "AdapterDetails");
    let (watts, voltage_mv, current_ma, is_wireless) = if let Some(ad) = adapter {
        let w = cf_utils::cfdict_get_i64(ad, "Watts").unwrap_or(0) as u32;
        let v = cf_utils::cfdict_get_i64(ad, "AdapterVoltage").unwrap_or(0) as u32;
        let a = cf_utils::cfdict_get_i64(ad, "Current").unwrap_or(0) as u32;
        let wireless = cf_utils::cfdict_get_bool(ad, "IsWireless").unwrap_or(false);
        (w, v, a, wireless)
    } else {
        (0, 0, 0, false)
    };

    cf_utils::cf_release(props as _);

    Some(AdapterInfo {
        connected: true,
        watts,
        voltage_mv,
        current_ma,
        is_wireless,
    })
}

/// Read USB power output per port from PowerOutDetails in AppleSmartBattery.
pub fn read_usb_power_out_per_port() -> Vec<UsbPortPower> {
    unsafe { read_usb_power_per_port_inner().unwrap_or_default() }
}

unsafe fn read_usb_power_per_port_inner() -> Option<Vec<UsbPortPower>> {
    use core_foundation_sys::array::CFArrayRef;

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

    // PowerOutDetails carries the measured watts per port, but the battery
    // firmware only lists the port(s) currently winning USB-PD negotiation —
    // a low-power charger drops out the instant another port ramps up. We read
    // it for watts, then fuse with PortControllerInfo (below) for presence.
    let mut watts_by_port: std::collections::HashMap<u32, (f32, u32)> =
        std::collections::HashMap::new();
    let arr_ref = cf_utils::cfdict_get(dict, "PowerOutDetails");
    if !arr_ref.is_null() {
        let arr = arr_ref as CFArrayRef;
        let count = cf_utils::cfarray_len(arr);
        for i in 0..count {
            let entry = cf_utils::cfarray_get(arr, i);
            if entry.is_null() {
                continue;
            }
            let d = entry as CFDictionaryRef;
            let port_idx = cf_utils::cfdict_get_i64(d, "PortIndex").unwrap_or(-1);
            if port_idx < 0 {
                continue;
            }
            let watts_mw = cf_utils::cfdict_get_i64(d, "Watts").unwrap_or(0);
            let pd_mw = cf_utils::cfdict_get_i64(d, "PDPowermW").unwrap_or(0);
            let power_mw = if watts_mw > 0 { watts_mw } else { pd_mw };
            let loc_id = cf_utils::cfdict_get_i64(d, "LocationID").unwrap_or(0) as u32;
            watts_by_port.insert(port_idx as u32, (power_mw as f32 / 1000.0, loc_id));
        }
    }

    // PortControllerInfo lists every physical port every poll. The FetStatus
    // high bit (0x80) means the Mac is sourcing power out of that port, which
    // is the reliable "a device is charging here" signal independent of the
    // PowerOutDetails prioritization. Index in this array == PortIndex above.
    let mut ports: Vec<UsbPortPower> = Vec::new();
    let mut sourced_indices: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let pci_ref = cf_utils::cfdict_get(dict, "PortControllerInfo");
    if !pci_ref.is_null() {
        let pci = pci_ref as CFArrayRef;
        let pci_count = cf_utils::cfarray_len(pci);
        for i in 0..pci_count {
            let entry = cf_utils::cfarray_get(pci, i);
            if entry.is_null() {
                continue;
            }
            let d = entry as CFDictionaryRef;
            let fet = cf_utils::cfdict_get_i64(d, "PortControllerFetStatus").unwrap_or(0);
            if fet & 0x80 == 0 {
                continue; // not sourcing power out of this port
            }
            let idx = i as u32;
            sourced_indices.insert(idx);
            let (power_w, location_id) = watts_by_port.get(&idx).copied().unwrap_or((0.0, 0));
            ports.push(UsbPortPower {
                port_index: idx,
                power_w,
                location_id,
                sourcing: true,
            });
        }
    }

    // Defensive fallback: if PortControllerInfo is unavailable (older Macs),
    // fall back to whatever PowerOutDetails reported so we don't regress.
    for (idx, (power_w, location_id)) in watts_by_port {
        if !sourced_indices.contains(&idx) {
            ports.push(UsbPortPower {
                port_index: idx,
                power_w,
                location_id,
                sourcing: true,
            });
        }
    }

    cf_utils::cf_release(props as _);
    Some(ports)
}
