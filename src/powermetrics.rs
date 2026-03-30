use crate::types::{DiskInfo, NetworkInfo};
use std::collections::HashMap;

/// Parsed cumulative byte counters from `netstat -ib`.
pub type NetCounters = HashMap<String, (u64, u64)>; // iface → (bytes_in, bytes_out)

/// Read cumulative network byte counters (no sudo needed).
pub fn read_net_counters() -> NetCounters {
    let output = match std::process::Command::new("netstat").args(["-ib"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return HashMap::new(),
    };

    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            (cols.len() >= 11 && cols[0] != "lo0" && cols[2].starts_with("<Link")).then_some(())?;
            let ibytes: u64 = cols[6].parse().unwrap_or(0);
            let obytes: u64 = cols[9].parse().unwrap_or(0);
            (ibytes > 0 || obytes > 0).then(|| (cols[0].to_string(), ibytes, obytes))
        })
        .fold(HashMap::new(), |mut acc, (iface, ib, ob)| {
            let entry = acc.entry(iface).or_insert((0, 0));
            entry.0 = entry.0.max(ib);
            entry.1 = entry.1.max(ob);
            acc
        })
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
