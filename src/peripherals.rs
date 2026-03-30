use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::types::{BluetoothDevice, PowerAssertion, UsbDevice, WifiInfo};
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use core_foundation_sys::number::CFNumberRef;

// ── USB / Thunderbolt enumeration ────────────────────────────────────────────

pub fn list_usb_devices() -> Vec<UsbDevice> {
    unsafe { list_usb_inner().unwrap_or_default() }
}

unsafe fn list_usb_inner() -> Option<Vec<UsbDevice>> {
    let matching = IOServiceMatching(b"IOUSBHostDevice\0".as_ptr() as *const i8);
    if matching.is_null() {
        return None;
    }

    let mut iter: u32 = 0;
    if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
        return None;
    }

    let mut devices = Vec::new();
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }

        let mut name_buf = [0i8; 128];
        let name = if IORegistryEntryGetName(entry, name_buf.as_mut_ptr()) == 0 {
            std::ffi::CStr::from_ptr(name_buf.as_ptr())
                .to_string_lossy()
                .into_owned()
        } else {
            "Unknown".into()
        };

        let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
        let mut vendor_id: u32 = 0;
        let mut product_id: u32 = 0;
        let mut power_ma: Option<u32> = None;
        let mut speed: u32 = 0;

        if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            vendor_id = cf_utils::cfdict_get_i64(dict, "idVendor").unwrap_or(0) as u32;
            product_id = cf_utils::cfdict_get_i64(dict, "idProduct").unwrap_or(0) as u32;
            power_ma = cf_utils::cfdict_get_i64(dict, "USB Power")
                .or_else(|| cf_utils::cfdict_get_i64(dict, "bMaxPower"))
                .map(|v| v as u32);
            speed = cf_utils::cfdict_get_i64(dict, "Device Speed").unwrap_or(0) as u32;
            cf_utils::cf_release(props as _);
        }

        let (bytes_read, bytes_written) = find_storage_stats(entry);

        IOObjectRelease(entry);

        devices.push(UsbDevice {
            name,
            vendor_id,
            product_id,
            power_ma,
            speed,
            bytes_read,
            bytes_written,
        });
    }

    IOObjectRelease(iter);
    Some(devices)
}

/// Walk child tree of a USB device to find IOBlockStorageDriver Statistics.
unsafe fn find_storage_stats(entry: u32) -> (u64, u64) {
    const KIO_REGISTRY_ITERATE_RECURSIVELY: u32 = 1;
    let mut child_iter: u32 = 0;
    let plane = b"IOService\0".as_ptr() as *const i8;
    if IORegistryEntryCreateIterator(entry, plane, KIO_REGISTRY_ITERATE_RECURSIVELY, &mut child_iter) != 0 {
        return (0, 0);
    }
    let mut result = (0u64, 0u64);
    loop {
        let child = IOIteratorNext(child_iter);
        if child == 0 { break; }
        let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
        if IORegistryEntryCreateCFProperties(child, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            let stats = cf_utils::cfdict_get(dict, "Statistics");
            if !stats.is_null() {
                let stats_dict = stats as CFDictionaryRef;
                let br = cf_utils::cfdict_get_i64(stats_dict, "Bytes (Read)").unwrap_or(0) as u64;
                let bw = cf_utils::cfdict_get_i64(stats_dict, "Bytes (Write)").unwrap_or(0) as u64;
                if br > 0 || bw > 0 {
                    result = (br, bw);
                }
            }
            cf_utils::cf_release(props as _);
        }
        IOObjectRelease(child);
        if result.0 > 0 || result.1 > 0 { break; }
    }
    IOObjectRelease(child_iter);
    result
}

// ── Power assertions ─────────────────────────────────────────────────────────

pub fn list_power_assertions() -> Vec<PowerAssertion> {
    unsafe { list_assertions_inner().unwrap_or_default() }
}

unsafe fn list_assertions_inner() -> Option<Vec<PowerAssertion>> {
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;

    let mut dict: CFDictionaryRef = std::ptr::null();
    let kr = IOPMCopyAssertionsByProcess(&mut dict);
    if kr != 0 || dict.is_null() {
        return None;
    }

    let mut result = Vec::new();

    // dict: { PID (CFNumber) -> CFArray of assertion dicts }
    // We iterate using CFDictionaryGetKeysAndValues
    let count = core_foundation_sys::dictionary::CFDictionaryGetCount(dict);
    if count == 0 {
        cf_utils::cf_release(dict as _);
        return Some(result);
    }

    let mut keys = vec![std::ptr::null(); count as usize];
    let mut vals = vec![std::ptr::null(); count as usize];
    core_foundation_sys::dictionary::CFDictionaryGetKeysAndValues(
        dict,
        keys.as_mut_ptr(),
        vals.as_mut_ptr(),
    );

    for i in 0..count as usize {
        let pid_ref = keys[i] as CFNumberRef;
        let pid_cf: CFNumber = TCFType::wrap_under_get_rule(pid_ref);
        let pid = pid_cf.to_i64().unwrap_or(0);

        let arr = vals[i] as CFArrayRef;
        let arr_len = cf_utils::cfarray_len(arr);

        for j in 0..arr_len {
            let entry = cf_utils::cfarray_get(arr, j) as CFDictionaryRef;
            let atype = cf_utils::cfdict_get_string(entry, "AssertionType")
                .or_else(|| cf_utils::cfdict_get_string(entry, "AssertType"))
                .unwrap_or_default();
            let aname = cf_utils::cfdict_get_string(entry, "AssertName").unwrap_or_default();

            result.push(PowerAssertion {
                name: aname,
                assertion_type: atype,
                pid,
            });
        }
    }

    cf_utils::cf_release(dict as _);
    Some(result)
}

// ── WiFi info via system_profiler ────────────────────────────────────────────

pub fn read_wifi_info() -> WifiInfo {
    let output = match std::process::Command::new("system_profiler")
        .args(["SPAirPortDataType", "-detailLevel", "basic"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return WifiInfo::default(),
    };

    let mut info = WifiInfo::default();

    for line in output.lines() {
        let l = line.trim();
        if l.starts_with("Status:") {
            info.connected = l.contains("Connected");
        } else if l.starts_with("PHY Mode:") {
            info.phy_mode = l.strip_prefix("PHY Mode:").unwrap_or("").trim().to_string();
        } else if l.starts_with("Channel:") {
            info.channel = l.strip_prefix("Channel:").unwrap_or("").trim().to_string();
        } else if l.starts_with("Signal / Noise:") {
            // "Signal / Noise: -57 dBm / -97 dBm"
            let after_colon = l.split(':').nth(1).unwrap_or("");
            let halves: Vec<&str> = after_colon.split('/').collect();
            if halves.len() >= 2 {
                info.rssi_dbm = halves[0]
                    .trim()
                    .replace(" dBm", "")
                    .trim()
                    .parse()
                    .unwrap_or(0);
                info.noise_dbm = halves[1]
                    .trim()
                    .replace(" dBm", "")
                    .trim()
                    .parse()
                    .unwrap_or(0);
            }
        } else if l.starts_with("Tx Rate:") {
            if let Some(v) = l.strip_prefix("Tx Rate:") {
                info.tx_rate_mbps = v.trim().replace(" Mbps", "").trim().parse().unwrap_or(0.0);
            }
        } else if l.starts_with("MCS Index:") {
            // already have channel/rate, skip
        }
        // Stop parsing after the first "Other Local" section
        if l.starts_with("Other Local Wi-Fi") {
            break;
        }
    }

    // WiFi power estimate: ~0.1W idle, up to ~0.8W active, scales with signal strength
    // Weaker signal = more tx power needed
    if info.connected {
        let signal_quality = ((info.rssi_dbm + 100).max(0).min(60)) as f32 / 60.0;
        info.estimated_power_w = 0.1 + (1.0 - signal_quality) * 0.7;
    }

    info
}

// ── Bluetooth connected devices via system_profiler ──────────────────────────

pub fn read_bluetooth_devices() -> Vec<BluetoothDevice> {
    let output = match std::process::Command::new("system_profiler")
        .args(["SPBluetoothDataType", "-detailLevel", "basic"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };

    let mut devices = Vec::new();
    let mut current: Option<BluetoothDevice> = None;
    let mut in_connected = false;

    for line in output.lines() {
        let trimmed = line.trim();
        let leading_spaces = line.len() - line.trim_start().len();

        // Section headers
        if trimmed == "Connected:" {
            in_connected = true;
            continue;
        }
        if trimmed == "Not Connected:" {
            // Flush last device from Connected section
            if let Some(dev) = current.take() {
                devices.push(dev);
            }
            in_connected = false;
            continue;
        }

        if !in_connected {
            continue;
        }

        // Device name: indented line ending with ':', indent ~10
        if trimmed.ends_with(':')
            && leading_spaces >= 8
            && leading_spaces <= 12
            && !trimmed.contains("Battery")
            && !trimmed.contains("Version")
        {
            if let Some(dev) = current.take() {
                devices.push(dev);
            }
            current = Some(BluetoothDevice {
                name: trimmed.trim_end_matches(':').to_string(),
                connected: true,
                ..Default::default()
            });
        } else if let Some(ref mut dev) = current {
            if trimmed.starts_with("Minor Type:") {
                dev.minor_type = trimmed
                    .strip_prefix("Minor Type:")
                    .unwrap_or("")
                    .trim()
                    .to_string();
            } else if trimmed.contains("Battery Level:") {
                // "Case Battery Level: 7%" → ("Case", "7%")
                let label = trimmed
                    .split("Battery Level")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let value = trimmed.split(':').nth(1).unwrap_or("").trim().to_string();
                dev.batteries.push((label, value));
            }
        }
    }
    // Flush last device if we were still in Connected section
    if in_connected {
        if let Some(dev) = current.take() {
            devices.push(dev);
        }
    }

    devices
}
