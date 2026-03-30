use crate::types::{DiskInfo, NetworkInfo};
use std::collections::HashMap;

/// Parsed cumulative byte counters per network interface.
pub type NetCounters = HashMap<String, (u64, u64)>; // iface → (bytes_in, bytes_out)

#[repr(C)]
struct IfData {
    ifi_type: u8,
    ifi_typelen: u8,
    ifi_physical: u8,
    ifi_addrlen: u8,
    ifi_hdrlen: u8,
    ifi_recvquota: u8,
    ifi_xmitquota: u8,
    ifi_unused1: u8,
    ifi_mtu: u32,
    ifi_metric: u32,
    ifi_baudrate: u32,
    ifi_ipackets: u32,
    ifi_ierrors: u32,
    ifi_opackets: u32,
    ifi_oerrors: u32,
    ifi_collisions: u32,
    ifi_ibytes: u32,
    ifi_obytes: u32,
    ifi_imcasts: u32,
    ifi_omcasts: u32,
    ifi_iqdrops: u32,
    ifi_noproto: u32,
    ifi_recvtiming: u32,
    ifi_xmittiming: u32,
    ifi_lastchange: libc::timeval,
}

/// Read cumulative network byte counters via getifaddrs (no subprocess).
pub fn read_net_counters() -> NetCounters {
    let mut result = HashMap::new();
    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return result;
        }
        let mut cur = addrs;
        while !cur.is_null() {
            let entry = &*cur;
            cur = entry.ifa_next;

            if entry.ifa_addr.is_null() || entry.ifa_data.is_null() {
                continue;
            }
            // AF_LINK = 18 on macOS
            if (*entry.ifa_addr).sa_family as i32 != libc::AF_LINK {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name).to_string_lossy();
            if name == "lo0" {
                continue;
            }
            let data = &*(entry.ifa_data as *const IfData);
            let ib = data.ifi_ibytes as u64;
            let ob = data.ifi_obytes as u64;
            if ib > 0 || ob > 0 {
                let e = result.entry(name.into_owned()).or_insert((0, 0));
                e.0 = e.0.max(ib);
                e.1 = e.1.max(ob);
            }
        }
        libc::freeifaddrs(addrs);
    }
    result
}

/// Compute network rates from two counter snapshots.
pub fn compute_net_rates(prev: &NetCounters, cur: &NetCounters, dt_s: f64) -> NetworkInfo {
    if dt_s <= 0.0 || prev.is_empty() {
        return NetworkInfo::default();
    }
    let (total_in, total_out) = cur
        .iter()
        .filter_map(|(iface, &(ci, co))| {
            prev.get(iface)
                .map(|&(pi, po)| (ci.saturating_sub(pi), co.saturating_sub(po)))
        })
        .fold((0u64, 0u64), |(ai, ao), (di, do_)| (ai + di, ao + do_));
    NetworkInfo {
        bytes_in_per_sec: total_in as f64 / dt_s,
        bytes_out_per_sec: total_out as f64 / dt_s,
    }
}

/// Cumulative disk byte counters from IOBlockStorageDriver Statistics.
pub type DiskCounters = (u64, u64); // (bytes_read, bytes_written)

/// Read cumulative disk byte counters from IORegistry (no subprocess needed).
pub fn read_disk_counters() -> DiskCounters {
    use crate::cf_utils;
    use crate::iokit_ffi::*;
    unsafe {
        let matching = IOServiceMatching(b"IOBlockStorageDriver\0".as_ptr() as *const i8);
        if matching.is_null() { return (0, 0); }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 { return (0, 0); }
        let mut total_read: u64 = 0;
        let mut total_write: u64 = 0;
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 { break; }
            let mut props = std::ptr::null_mut();
            if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
                && !props.is_null()
            {
                let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
                let stats = cf_utils::cfdict_get(dict, "Statistics");
                if !stats.is_null() {
                    let sd = stats as core_foundation_sys::dictionary::CFDictionaryRef;
                    total_read += cf_utils::cfdict_get_i64(sd, "Bytes (Read)").unwrap_or(0) as u64;
                    total_write += cf_utils::cfdict_get_i64(sd, "Bytes (Write)").unwrap_or(0) as u64;
                }
                cf_utils::cf_release(props as _);
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
        (total_read, total_write)
    }
}

/// Compute disk rates from two counter snapshots.
pub fn compute_disk_rates(prev: &DiskCounters, cur: &DiskCounters, dt_s: f64) -> DiskInfo {
    if dt_s <= 0.0 { return DiskInfo::default(); }
    DiskInfo {
        read_bytes_per_sec: cur.0.saturating_sub(prev.0) as f64 / dt_s,
        write_bytes_per_sec: cur.1.saturating_sub(prev.1) as f64 / dt_s,
    }
}
