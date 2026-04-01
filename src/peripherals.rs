use crate::cf_utils;
use crate::iokit_ffi::*;
use crate::types::{BluetoothDevice, EthernetInfo, PowerAssertion, UsbDevice, WifiInfo};
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
        let mut location_id: u32 = 0;

        if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            vendor_id = cf_utils::cfdict_get_i64(dict, "idVendor").unwrap_or(0) as u32;
            product_id = cf_utils::cfdict_get_i64(dict, "idProduct").unwrap_or(0) as u32;
            power_ma = cf_utils::cfdict_get_i64(dict, "UsbPowerSinkAllocation")
                .or_else(|| cf_utils::cfdict_get_i64(dict, "USB Power"))
                .or_else(|| cf_utils::cfdict_get_i64(dict, "bMaxPower"))
                .map(|v| v as u32);
            speed = cf_utils::cfdict_get_i64(dict, "Device Speed").unwrap_or(0) as u32;
            location_id = cf_utils::cfdict_get_i64(dict, "locationID").unwrap_or(0) as u32;
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
            location_id,
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
    if IORegistryEntryCreateIterator(
        entry,
        plane,
        KIO_REGISTRY_ITERATE_RECURSIVELY,
        &mut child_iter,
    ) != 0
    {
        return (0, 0);
    }
    let mut result = (0u64, 0u64);
    loop {
        let child = IOIteratorNext(child_iter);
        if child == 0 {
            break;
        }
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
        if result.0 > 0 || result.1 > 0 {
            break;
        }
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

// ── WiFi info via CoreWLAN (ObjC runtime) + ipconfig (SSID fallback) ─────────

#[link(name = "CoreWLAN", kind = "framework")]
extern "C" {}

extern "C" {
    fn objc_getClass(name: *const i8) -> *mut libc::c_void;
    fn sel_registerName(name: *const i8) -> *mut libc::c_void;
    fn objc_msgSend(obj: *mut libc::c_void, sel: *mut libc::c_void, ...) -> *mut libc::c_void;
}

// ── Ethernet detection via getifaddrs ─────────────────────────────────────────

pub fn read_ethernet_info(wifi_iface: &str) -> EthernetInfo {
    #[repr(C)]
    struct IfData {
        ifi_type: u8,
        _pad: [u8; 7],
        ifi_mtu: u32,
        ifi_metric: u32,
        ifi_baudrate: u32,
    }

    const IFT_ETHER: u8 = 6;

    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return EthernetInfo::default();
        }

        let mut best: Option<(String, u32)> = None;
        let mut cur = addrs;
        while !cur.is_null() {
            let entry = &*cur;
            cur = entry.ifa_next;

            if entry.ifa_addr.is_null() || entry.ifa_data.is_null() {
                continue;
            }
            if (*entry.ifa_addr).sa_family as i32 != libc::AF_LINK {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name).to_string_lossy();
            if !name.starts_with("en") {
                continue;
            }
            // WiFi interfaces also report IFT_ETHER at link layer; skip the
            // known WiFi interface so we only detect real Ethernet ports.
            if !wifi_iface.is_empty() && name == wifi_iface {
                continue;
            }
            let data = &*(entry.ifa_data as *const IfData);
            if data.ifi_type != IFT_ETHER {
                continue;
            }
            let flags = entry.ifa_flags;
            let up = (flags & libc::IFF_UP as u32) != 0;
            let running = (flags & libc::IFF_RUNNING as u32) != 0;
            if !up || !running {
                continue;
            }
            let speed_mbps = data.ifi_baudrate / 1_000_000;
            if best.is_none() || speed_mbps > best.as_ref().unwrap().1 {
                best = Some((name.into_owned(), speed_mbps));
            }
        }
        libc::freeifaddrs(addrs);

        match best {
            Some((iface, speed)) => EthernetInfo {
                connected: true,
                interface_name: iface,
                link_speed_mbps: speed,
            },
            None => EthernetInfo::default(),
        }
    }
}

// ── WiFi via CoreWLAN ─────────────────────────────────────────────────────────

fn phy_mode_str(mode: i64) -> &'static str {
    match mode {
        1 => "802.11a",
        2 => "802.11b",
        3 => "802.11g",
        4 => "802.11n",
        5 => "802.11ac",
        6 => "802.11ax",
        7 => "802.11be",
        _ => "",
    }
}

fn read_wifi_ssid_ipconfig(iface_name: &str) -> Option<String> {
    let output = std::process::Command::new("ipconfig")
        .args(["getsummary", iface_name])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(ssid) = trimmed.strip_prefix("SSID : ") {
            let s = ssid.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

pub fn read_wifi_info() -> WifiInfo {
    unsafe {
        let no_hardware = WifiInfo {
            phy_mode: "none".into(),
            ..Default::default()
        };

        let cls = objc_getClass(b"CWWiFiClient\0".as_ptr() as _);
        if cls.is_null() {
            return no_hardware;
        }
        let sel_shared = sel_registerName(b"sharedWiFiClient\0".as_ptr() as _);
        let client = objc_msgSend(cls, sel_shared);
        if client.is_null() {
            return no_hardware;
        }
        let sel_iface = sel_registerName(b"interface\0".as_ptr() as _);
        let iface = objc_msgSend(client, sel_iface);
        if iface.is_null() {
            return no_hardware;
        }

        // Get the actual interface name (e.g. en0 on laptops, en1 on Mac Studio)
        // so the ipconfig fallback queries the right interface.
        let sel_iface_name = sel_registerName(b"interfaceName\0".as_ptr() as _);
        let iface_name_ns = objc_msgSend(iface, sel_iface_name);
        let iface_name = if !iface_name_ns.is_null() {
            cf_utils::cfstring_to_string(iface_name_ns as _).unwrap_or_default()
        } else {
            String::new()
        };

        // ssid requires location permission (nil for unsigned binaries since Sonoma)
        // Fall back to ipconfig getsummary for SSID
        let sel_ssid = sel_registerName(b"ssid\0".as_ptr() as _);
        let ssid_ns = objc_msgSend(iface, sel_ssid);
        let ssid = if !ssid_ns.is_null() {
            cf_utils::cfstring_to_string(ssid_ns as _).unwrap_or_default()
        } else if !iface_name.is_empty() {
            read_wifi_ssid_ipconfig(&iface_name).unwrap_or_default()
        } else {
            read_wifi_ssid_ipconfig("en0").unwrap_or_default()
        };

        let connected = !ssid.is_empty();
        if !connected {
            return WifiInfo {
                interface_name: iface_name,
                ..Default::default()
            };
        }

        let mut info = WifiInfo {
            connected: true,
            interface_name: iface_name,
            ssid,
            ..Default::default()
        };

        let sel_rssi = sel_registerName(b"rssiValue\0".as_ptr() as _);
        info.rssi_dbm = objc_msgSend(iface, sel_rssi) as i64 as i32;

        let sel_noise = sel_registerName(b"noiseMeasurement\0".as_ptr() as _);
        info.noise_dbm = objc_msgSend(iface, sel_noise) as i64 as i32;

        let sel_tx = sel_registerName(b"transmitRate\0".as_ptr() as _);
        let tx_raw = objc_msgSend(iface, sel_tx) as u64;
        info.tx_rate_mbps = f64::from_bits(tx_raw) as f32;

        let sel_phy = sel_registerName(b"activePHYMode\0".as_ptr() as _);
        let phy_mode = objc_msgSend(iface, sel_phy) as i64;
        info.phy_mode = phy_mode_str(phy_mode).to_string();

        let sel_chan = sel_registerName(b"wlanChannel\0".as_ptr() as _);
        let channel = objc_msgSend(iface, sel_chan);
        if !channel.is_null() {
            let sel_num = sel_registerName(b"channelNumber\0".as_ptr() as _);
            let sel_band = sel_registerName(b"channelBand\0".as_ptr() as _);
            let ch_num = objc_msgSend(channel, sel_num) as i64;
            let ch_band = objc_msgSend(channel, sel_band) as i64;
            let band_str = match ch_band {
                1 => "2GHz",
                2 => "5GHz",
                3 => "6GHz",
                _ => "",
            };
            if !band_str.is_empty() {
                info.channel = format!("{} ({})", ch_num, band_str);
            } else {
                info.channel = format!("{}", ch_num);
            }
        }

        // WiFi power estimate: ~0.1W idle, up to ~0.8W active, scales with signal strength
        let signal_quality = ((info.rssi_dbm + 100).max(0).min(60)) as f32 / 60.0;
        info.estimated_power_w = 0.1 + (1.0 - signal_quality) * 0.7;

        info
    }
}

// ── Bluetooth devices via pmset -g accps ─────────────────────────────────────

pub fn read_bluetooth_devices() -> Vec<BluetoothDevice> {
    let output = match std::process::Command::new("pmset")
        .args(["-g", "accps"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };

    struct Entry {
        name: String,
        id: u64,
        pct: String,
        status: String,
    }

    let mut raw: Vec<Entry> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-InternalBattery")
            || trimmed.starts_with("Now drawing")
            || trimmed.is_empty()
        {
            continue;
        }
        if let Some(name_end) = trimmed.find(" (id=") {
            let name = trimmed[1..name_end].trim_start_matches('-').trim();
            let id: u64 = trimmed[name_end + 5..]
                .split(')')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let after_tab = trimmed.split('\t').nth(1).unwrap_or("");
            let pct = after_tab.split('%').next().unwrap_or("").trim();

            let segments: Vec<&str> = after_tab.split(';').collect();
            let is_charging = segments.len() >= 2
                && segments[1].contains("charging")
                && !segments[1].contains("discharging");
            let mut status = String::new();
            if is_charging {
                let time_seg = segments.get(2).unwrap_or(&"").trim();
                let time = time_seg
                    .replace("remaining", "")
                    .replace("present: true", "")
                    .trim()
                    .to_string();
                if !time.is_empty() && time != "0:00" {
                    status = format!("charging, {} left", time);
                } else if pct.parse::<u32>().unwrap_or(0) >= 100 {
                    status = "charged".to_string();
                } else {
                    status = "charging".to_string();
                }
            }

            if !name.is_empty() && !pct.is_empty() {
                raw.push(Entry {
                    name: name.to_string(),
                    id,
                    pct: format!("{}%", pct),
                    status,
                });
            }
        }
    }

    raw.sort_by_key(|r| r.id);

    let mut name_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for e in &raw {
        if !e.name.ends_with(" Case") {
            *name_counts.entry(e.name.clone()).or_insert(0) += 1;
        }
    }

    let mut devices: Vec<BluetoothDevice> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for e in &raw {
        let display_name = if e.name.ends_with(" Case") {
            e.name.clone()
        } else {
            let count = name_counts.get(&e.name).copied().unwrap_or(1);
            if count >= 2 {
                let nth = seen.entry(e.name.clone()).or_insert(0);
                let side = if *nth == 0 { "Right" } else { "Left" };
                *nth += 1;
                format!("{} ({})", e.name, side)
            } else {
                e.name.clone()
            }
        };

        let battery_str = if e.status.is_empty() {
            e.pct.clone()
        } else {
            format!("{}, {}", e.pct, e.status)
        };

        devices.push(BluetoothDevice {
            name: display_name,
            minor_type: String::new(),
            connected: true,
            batteries: vec![(String::new(), battery_str)],
        });
    }

    devices
}
