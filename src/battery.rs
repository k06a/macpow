use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::types::{AdapterInfo, BatteryInfo, UsbPortPower};
use core_foundation_sys::dictionary::CFDictionaryRef;
use std::ffi::CStr;

pub fn read_battery() -> BatteryInfo {
    read_battery_readings()
        .map(|r| battery_info(&r))
        .unwrap_or_default()
}

pub fn read_adapter() -> AdapterInfo {
    unsafe { read_adapter_inner().unwrap_or_default() }
}

/// Capacity and temperature readings of the battery's fuel gauge, as one
/// registry dictionary publishes them. `None` means it has no such key.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct GaugeValues {
    /// "NominalChargeCapacity"
    nominal_charge_mah: Option<f64>,
    /// "DesignCapacity"
    design_capacity_mah: Option<f64>,
    /// "AppleRawMaxCapacity": the charge the battery holds when full today.
    raw_max_capacity_mah: Option<f64>,
    /// "FccComp1": the same, compensated by the gauge; preferred for health.
    fcc_comp1_mah: Option<f64>,
    /// "Temperature", in hundredths of a degree Celsius (3045 = 30.45 °C).
    temperature_centi_c: Option<f64>,
}

/// Everything `read_battery` takes from the registry, before any arithmetic.
/// `None` means the key is not published.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct BatteryReadings {
    voltage_mv: Option<f64>,
    amperage_ma: Option<f64>,
    current_capacity: Option<i64>,
    max_capacity: Option<i64>,
    is_charging: Option<bool>,
    external_connected: Option<bool>,
    time_remaining_min: Option<i64>,
    cycle_count: Option<i64>,
    /// The places the gauge values have lived, in the order they are tried:
    /// AppleSmartBattery itself (older macOS), its "BatteryData" dictionary,
    /// and the "BatteryData" of the AppleSmartBatteryPack node (macOS 27 keeps
    /// them only in the last two).
    gauge_places: [GaugeValues; 3],
}

impl BatteryReadings {
    /// The value chosen by `field` from the first place that publishes it.
    fn gauge(&self, field: impl Fn(&GaugeValues) -> Option<f64>) -> Option<f64> {
        self.gauge_places.iter().find_map(field)
    }
}

/// Turns the registry readings into the reported battery state.
///
/// A gauge value that no place publishes is reported as 0, meaning "unknown":
/// it is never replaced by another quantity or by a plausible default.
fn battery_info(r: &BatteryReadings) -> BatteryInfo {
    let voltage_mv = r.voltage_mv.unwrap_or(0.0);
    let amperage_ma = r.amperage_ma.unwrap_or(0.0);
    let current_cap = r.current_capacity.unwrap_or(0);
    let max_cap = r.max_capacity.unwrap_or(1);
    let external = r.external_connected.unwrap_or(false);
    let raw_time = r.time_remaining_min.unwrap_or(-1);

    let nominal_charge_mah = r.gauge(|g| g.nominal_charge_mah).unwrap_or(0.0);
    let design_capacity_mah = r.gauge(|g| g.design_capacity_mah).unwrap_or(0.0);
    let max_capacity_mah = r.gauge(|g| g.raw_max_capacity_mah).unwrap_or(0.0);
    let temperature_c = r.gauge(|g| g.temperature_centi_c).unwrap_or(0.0) / 100.0;

    let full_charge_mah = r
        .gauge(|g| g.fcc_comp1_mah)
        .filter(|fcc| *fcc > 0.0)
        .unwrap_or(max_capacity_mah);
    // Health needs both capacities. Without the design capacity it is unknown
    // (0, which the TUI leaves out) rather than a 100 % nobody measured, and
    // without a full-charge capacity the division gives the same 0.
    let health_pct = if design_capacity_mah > 0.0 {
        full_charge_mah / design_capacity_mah * 100.0
    } else {
        0.0
    };

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

    BatteryInfo {
        present: true,
        charging: r.is_charging.unwrap_or(false),
        voltage_mv,
        amperage_ma,
        drain_w,
        capacity_wh: nominal_charge_mah * voltage_mv / 1_000_000.0,
        current_capacity: current_cap,
        max_capacity: max_cap,
        percent,
        time_remaining_min: time_remaining,
        external_connected: external,
        temperature_c,
        cycle_count: r.cycle_count.unwrap_or(0),
        design_capacity_mah,
        max_capacity_mah,
        health_pct,
    }
}

/// Reads the battery values from the registry; `None` when the machine has no
/// AppleSmartBattery service.
fn read_battery_readings() -> Option<BatteryReadings> {
    let battery = copy_service_properties(c"AppleSmartBattery")?;
    // Absent on older macOS, where AppleSmartBattery has everything.
    let pack = copy_service_properties(c"AppleSmartBatteryPack");
    // SAFETY: both are dictionaries we own until the releases below, each
    // released exactly once; `readings_from` returns plain numbers that do
    // not point into them.
    unsafe {
        let readings = readings_from(battery, pack);
        cf_utils::cf_release(battery as _);
        if let Some(pack) = pack {
            cf_utils::cf_release(pack as _);
        }
        Some(readings)
    }
}

/// Copies the property dictionary of the first IOService of class
/// `class_name`; `None` when there is no such service. The caller owns the
/// dictionary and has to release it.
fn copy_service_properties(class_name: &CStr) -> Option<CFDictionaryRef> {
    // SAFETY: `class_name` is NUL-terminated. IOServiceGetMatchingService
    // consumes the matching dictionary, the service handle is released right
    // after its properties are copied, and `props` is used only when IOKit
    // reports success and filled it in.
    unsafe {
        let matching = IOServiceMatching(class_name.as_ptr());
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
        Some(props as CFDictionaryRef)
    }
}

/// Extracts the readings from the properties of AppleSmartBattery and, where
/// it exists, of AppleSmartBatteryPack.
///
/// # Safety
/// `battery` and `pack` must be valid CFDictionary references.
unsafe fn readings_from(
    battery: CFDictionaryRef,
    pack: Option<CFDictionaryRef>,
) -> BatteryReadings {
    BatteryReadings {
        voltage_mv: cf_utils::cfdict_get_f64(battery, "AppleRawBatteryVoltage")
            .or_else(|| cf_utils::cfdict_get_f64(battery, "Voltage")),
        amperage_ma: cf_utils::cfdict_get_f64(battery, "InstantAmperage")
            .or_else(|| cf_utils::cfdict_get_f64(battery, "Amperage")),
        current_capacity: cf_utils::cfdict_get_i64(battery, "CurrentCapacity"),
        max_capacity: cf_utils::cfdict_get_i64(battery, "MaxCapacity"),
        is_charging: cf_utils::cfdict_get_bool(battery, "IsCharging"),
        external_connected: cf_utils::cfdict_get_bool(battery, "ExternalConnected"),
        time_remaining_min: cf_utils::cfdict_get_i64(battery, "TimeRemaining"),
        cycle_count: cf_utils::cfdict_get_i64(battery, "CycleCount"),
        gauge_places: [
            gauge_values(Some(battery)),
            gauge_values(cf_utils::cfdict_get_dict(battery, "BatteryData")),
            gauge_values(pack.and_then(|pack| cf_utils::cfdict_get_dict(pack, "BatteryData"))),
        ],
    }
}

/// Reads the gauge keys of one dictionary; all `None` without a dictionary.
/// These keys have already moved between macOS releases; the cf_utils getters
/// read one that comes back with a different type as missing.
///
/// # Safety
/// `dict`, when given, must be a valid CFDictionary reference.
unsafe fn gauge_values(dict: Option<CFDictionaryRef>) -> GaugeValues {
    let Some(dict) = dict else {
        return GaugeValues::default();
    };
    GaugeValues {
        nominal_charge_mah: cf_utils::cfdict_get_f64(dict, "NominalChargeCapacity"),
        design_capacity_mah: cf_utils::cfdict_get_f64(dict, "DesignCapacity"),
        raw_max_capacity_mah: cf_utils::cfdict_get_f64(dict, "AppleRawMaxCapacity"),
        fcc_comp1_mah: cf_utils::cfdict_get_f64(dict, "FccComp1"),
        temperature_centi_c: cf_utils::cfdict_get_f64(dict, "Temperature"),
    }
}

unsafe fn read_adapter_inner() -> Option<AdapterInfo> {
    let dict = copy_service_properties(c"AppleSmartBattery")?;
    let external = cf_utils::cfdict_get_bool(dict, "ExternalConnected").unwrap_or(false);

    if !external {
        cf_utils::cf_release(dict as _);
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

    cf_utils::cf_release(dict as _);

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

    let dict = copy_service_properties(c"AppleSmartBattery")?;

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

    cf_utils::cf_release(dict as _);
    Some(ports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    const NO_GAUGE: GaugeValues = GaugeValues {
        nominal_charge_mah: None,
        design_capacity_mah: None,
        raw_max_capacity_mah: None,
        fcc_comp1_mah: None,
        temperature_centi_c: None,
    };

    /// The values `ioreg` showed on a MacBook Pro (Mac17,7) with macOS 27.0.1:
    /// nothing from the gauge at the top level of AppleSmartBattery, a summary
    /// in its "BatteryData" and the full set in AppleSmartBatteryPack.
    fn macos_27() -> BatteryReadings {
        BatteryReadings {
            voltage_mv: Some(12014.0),
            amperage_ma: Some(-1759.0),
            current_capacity: Some(75),
            max_capacity: Some(100),
            is_charging: Some(false),
            external_connected: Some(false),
            time_remaining_min: Some(130),
            cycle_count: Some(241),
            gauge_places: [
                NO_GAUGE,
                GaugeValues {
                    nominal_charge_mah: Some(5726.0),
                    design_capacity_mah: Some(6249.0),
                    ..NO_GAUGE
                },
                GaugeValues {
                    nominal_charge_mah: Some(5726.0),
                    design_capacity_mah: Some(6249.0),
                    raw_max_capacity_mah: Some(5574.0),
                    fcc_comp1_mah: Some(5574.0),
                    temperature_centi_c: Some(3450.0),
                },
            ],
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "{actual} is not {expected}"
        );
    }

    #[test]
    fn macos_27_layout_is_read_from_the_nested_dictionaries() {
        let b = battery_info(&macos_27());
        assert!(b.present);
        assert_close(b.capacity_wh, 5726.0 * 12014.0 / 1_000_000.0);
        assert_close(b.temperature_c, 34.5);
        assert_close(b.design_capacity_mah, 6249.0);
        assert_close(b.max_capacity_mah, 5574.0);
        assert_close(b.health_pct, 5574.0 / 6249.0 * 100.0);
        // The values that did not move are passed through as before.
        assert_close(b.voltage_mv, 12014.0);
        assert_close(b.amperage_ma, -1759.0);
        assert_close(b.drain_w, 12014.0 * 1759.0 / 1_000_000.0);
        assert_close(b.percent, 75.0);
        assert_eq!((b.current_capacity, b.max_capacity), (75, 100));
        assert_eq!((b.time_remaining_min, b.cycle_count), (130, 241));
        assert!(!b.charging && !b.external_connected);
    }

    /// The layout the code read before macOS 27: the gauge keys at the top
    /// level, "FccComp1" in "BatteryData", no AppleSmartBatteryPack node. The
    /// numbers are made up; no machine with that layout was at hand.
    #[test]
    fn older_layout_with_top_level_keys_still_works() {
        let r = BatteryReadings {
            voltage_mv: Some(12450.0),
            gauge_places: [
                GaugeValues {
                    nominal_charge_mah: Some(4490.0),
                    design_capacity_mah: Some(4563.0),
                    raw_max_capacity_mah: Some(4382.0),
                    fcc_comp1_mah: None,
                    temperature_centi_c: Some(3033.0),
                },
                GaugeValues {
                    design_capacity_mah: Some(4563.0),
                    fcc_comp1_mah: Some(4338.0),
                    ..NO_GAUGE
                },
                NO_GAUGE,
            ],
            ..macos_27()
        };
        let b = battery_info(&r);
        assert_close(b.capacity_wh, 4490.0 * 12450.0 / 1_000_000.0);
        assert_close(b.temperature_c, 30.33);
        assert_close(b.design_capacity_mah, 4563.0);
        assert_close(b.max_capacity_mah, 4382.0);
        assert_close(b.health_pct, 4338.0 / 4563.0 * 100.0);
    }

    #[test]
    fn a_value_is_taken_from_the_first_place_that_publishes_it() {
        let place = |mah| GaugeValues {
            design_capacity_mah: Some(mah),
            ..NO_GAUGE
        };
        let mut r = macos_27();
        r.gauge_places = [place(1.0), place(2.0), place(3.0)];
        assert_eq!(r.gauge(|g| g.design_capacity_mah), Some(1.0));
        r.gauge_places = [NO_GAUGE, place(2.0), place(3.0)];
        assert_eq!(r.gauge(|g| g.design_capacity_mah), Some(2.0));
        r.gauge_places = [NO_GAUGE, NO_GAUGE, place(3.0)];
        assert_eq!(r.gauge(|g| g.design_capacity_mah), Some(3.0));
        r.gauge_places = [NO_GAUGE; 3];
        assert_eq!(r.gauge(|g| g.design_capacity_mah), None);
    }

    /// Desktop Macs can expose a phantom AppleSmartBattery whose values are
    /// all zero (see the battery row in app.rs, which recognises it by
    /// `max_capacity == 0`). Nothing else may look like a reading either.
    #[test]
    fn desktop_phantom_battery_reports_nothing_made_up() {
        let zeros = GaugeValues {
            nominal_charge_mah: Some(0.0),
            design_capacity_mah: Some(0.0),
            raw_max_capacity_mah: Some(0.0),
            fcc_comp1_mah: Some(0.0),
            temperature_centi_c: Some(0.0),
        };
        let r = BatteryReadings {
            voltage_mv: Some(0.0),
            amperage_ma: Some(0.0),
            current_capacity: Some(0),
            max_capacity: Some(0),
            is_charging: Some(false),
            external_connected: Some(true),
            time_remaining_min: Some(0),
            cycle_count: Some(0),
            gauge_places: [zeros, NO_GAUGE, NO_GAUGE],
        };
        let b = battery_info(&r);
        assert_eq!(b.max_capacity, 0);
        for value in [
            b.percent,
            b.drain_w,
            b.capacity_wh,
            b.temperature_c,
            b.design_capacity_mah,
            b.max_capacity_mah,
            b.health_pct,
        ] {
            assert_close(value, 0.0);
        }
        assert_eq!(b.time_remaining_min, -1);
    }

    #[test]
    fn health_is_unknown_without_a_design_capacity() {
        let mut r = macos_27();
        for place in &mut r.gauge_places {
            place.design_capacity_mah = None;
        }
        let b = battery_info(&r);
        assert_close(b.design_capacity_mah, 0.0);
        assert_close(b.health_pct, 0.0);
        // What was published is still reported.
        assert_close(b.max_capacity_mah, 5574.0);
        assert_close(b.capacity_wh, 5726.0 * 12014.0 / 1_000_000.0);
    }

    #[test]
    fn design_capacity_never_stands_in_for_the_full_charge_capacity() {
        let mut r = macos_27();
        for place in &mut r.gauge_places {
            place.raw_max_capacity_mah = None;
            place.fcc_comp1_mah = None;
        }
        let b = battery_info(&r);
        assert_close(b.design_capacity_mah, 6249.0);
        assert_close(b.max_capacity_mah, 0.0);
        assert_close(b.health_pct, 0.0);
    }

    #[test]
    fn health_falls_back_to_the_raw_capacity_when_fcc_comp1_is_unusable() {
        let mut r = macos_27();
        r.gauge_places[2].raw_max_capacity_mah = Some(5600.0);
        r.gauge_places[2].fcc_comp1_mah = Some(0.0);
        assert_close(battery_info(&r).health_pct, 5600.0 / 6249.0 * 100.0);
        r.gauge_places[2].fcc_comp1_mah = None;
        assert_close(battery_info(&r).health_pct, 5600.0 / 6249.0 * 100.0);
    }

    #[test]
    fn missing_values_are_zero_not_guesses() {
        let b = battery_info(&BatteryReadings::default());
        for value in [
            b.voltage_mv,
            b.amperage_ma,
            b.drain_w,
            b.capacity_wh,
            b.percent,
            b.temperature_c,
            b.design_capacity_mah,
            b.max_capacity_mah,
            b.health_pct,
        ] {
            assert_close(value, 0.0);
        }
        assert_eq!(b.time_remaining_min, -1);
    }

    fn cf_dict(pairs: &[(&str, CFType)]) -> CFDictionary<CFString, CFType> {
        let pairs: Vec<(CFString, CFType)> = pairs
            .iter()
            .map(|(key, value)| (CFString::new(key), value.clone()))
            .collect();
        CFDictionary::from_CFType_pairs(&pairs)
    }

    #[test]
    fn registry_values_of_another_type_read_as_missing() {
        let gauge = cf_dict(&[
            ("DesignCapacity", CFNumber::from(6249i64).as_CFType()),
            ("NominalChargeCapacity", CFString::new("5726").as_CFType()),
        ]);
        let battery = cf_dict(&[
            ("CycleCount", CFNumber::from(241i64).as_CFType()),
            ("BatteryData", gauge.as_CFType()),
        ]);
        let pack = cf_dict(&[("BatteryData", CFString::new("not a dict").as_CFType())]);
        // SAFETY: both dictionaries are valid and outlive the call.
        let r = unsafe {
            readings_from(
                battery.as_concrete_TypeRef(),
                Some(pack.as_concrete_TypeRef()),
            )
        };
        assert_eq!(r.cycle_count, Some(241));
        assert_eq!(r.gauge_places[0], NO_GAUGE);
        assert_eq!(
            r.gauge_places[1],
            GaugeValues {
                design_capacity_mah: Some(6249.0),
                ..NO_GAUGE
            }
        );
        assert_eq!(r.gauge_places[2], NO_GAUGE);
    }
}
