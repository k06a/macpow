use crate::process_utils::command_output_timeout;
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
    compute_net_rates_for(prev, cur, dt_s, |_| true)
}

/// Compute network rates for a single interface.
pub fn compute_net_rates_iface(
    prev: &NetCounters,
    cur: &NetCounters,
    dt_s: f64,
    iface: &str,
) -> NetworkInfo {
    let name = iface.to_string();
    compute_net_rates_for(prev, cur, dt_s, |n| n == &name)
}

fn compute_net_rates_for(
    prev: &NetCounters,
    cur: &NetCounters,
    dt_s: f64,
    filter: impl Fn(&String) -> bool,
) -> NetworkInfo {
    if dt_s <= 0.0 || prev.is_empty() {
        return NetworkInfo::default();
    }
    let (total_in, total_out) = cur
        .iter()
        .filter(|(iface, _)| filter(iface))
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
        if matching.is_null() {
            return (0, 0);
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return (0, 0);
        }
        let mut total_read: u64 = 0;
        let mut total_write: u64 = 0;
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            let mut props = std::ptr::null_mut();
            if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
                && !props.is_null()
            {
                let dict = props as core_foundation_sys::dictionary::CFDictionaryRef;
                let stats = cf_utils::cfdict_get(dict, "Statistics");
                if !stats.is_null() {
                    let sd = stats as core_foundation_sys::dictionary::CFDictionaryRef;
                    total_read += cf_utils::cfdict_get_i64(sd, "Bytes (Read)").unwrap_or(0) as u64;
                    total_write +=
                        cf_utils::cfdict_get_i64(sd, "Bytes (Write)").unwrap_or(0) as u64;
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
    if dt_s <= 0.0 {
        return DiskInfo::default();
    }
    DiskInfo {
        read_bytes_per_sec: cur.0.saturating_sub(prev.0) as f64 / dt_s,
        write_bytes_per_sec: cur.1.saturating_sub(prev.1) as f64 / dt_s,
    }
}

// ── Per-process network via nettop ───────────────────────────────────────────

/// Cumulative per-process network byte counters from nettop.
pub type ProcNetCounters = HashMap<i32, (u64, u64)>; // pid → (rx_bytes, tx_bytes)

/// Read per-process cumulative network bytes via `nettop -P -n -L 1 -x`.
/// Returns in ~18ms. Parses CSV: "name.pid,,iface,state,bytes_in,bytes_out,..."
pub fn read_proc_net_counters() -> ProcNetCounters {
    let output = match command_output_timeout(
        "/usr/bin/nettop",
        &["-P", "-n", "-L", "1", "-x"],
        std::time::Duration::from_millis(1000),
    ) {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return HashMap::new(),
    };

    let mut result = HashMap::new();
    for line in output.lines().skip(1) {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 7 {
            continue;
        }
        // Format: time,name.pid,iface,state,bytes_in,bytes_out,...
        let name_pid = cols[1].trim();
        let (name, pid_suffix) = match name_pid.rsplit_once('.') {
            Some(parts) => parts,
            None => continue,
        };
        if name.is_empty() || !pid_suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let pid: i32 = match pid_suffix.parse().ok() {
            Some(p) if p > 0 => p,
            _ => continue,
        };
        let rx: u64 = cols[4].trim().parse().unwrap_or(0);
        let tx: u64 = cols[5].trim().parse().unwrap_or(0);
        if rx > 0 || tx > 0 {
            let e = result.entry(pid).or_insert((0u64, 0u64));
            e.0 += rx;
            e.1 += tx;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_net_rates_aggregates_interfaces() {
        let prev = HashMap::from([
            ("en0".to_string(), (100u64, 200u64)),
            ("en1".to_string(), (50u64, 100u64)),
        ]);
        let cur = HashMap::from([
            ("en0".to_string(), (300u64, 500u64)),
            ("en1".to_string(), (100u64, 160u64)),
        ]);
        let rates = compute_net_rates(&prev, &cur, 2.0);
        assert_eq!(rates.bytes_in_per_sec, 125.0);
        assert_eq!(rates.bytes_out_per_sec, 180.0);
    }

    #[test]
    fn compute_net_rates_iface_filters_target_only() {
        let prev = HashMap::from([
            ("en0".to_string(), (100u64, 200u64)),
            ("en1".to_string(), (10u64, 20u64)),
        ]);
        let cur = HashMap::from([
            ("en0".to_string(), (160u64, 260u64)),
            ("en1".to_string(), (110u64, 220u64)),
        ]);
        let rates = compute_net_rates_iface(&prev, &cur, 2.0, "en0");
        assert_eq!(rates.bytes_in_per_sec, 30.0);
        assert_eq!(rates.bytes_out_per_sec, 30.0);
    }

    #[test]
    fn compute_disk_rates_handles_regular_and_zero_delta() {
        let rates = compute_disk_rates(&(100, 200), &(300, 500), 2.0);
        assert_eq!(rates.read_bytes_per_sec, 100.0);
        assert_eq!(rates.write_bytes_per_sec, 150.0);

        let zero = compute_disk_rates(&(300, 500), &(100, 200), 2.0);
        assert_eq!(zero.read_bytes_per_sec, 0.0);
        assert_eq!(zero.write_bytes_per_sec, 0.0);
    }
}
