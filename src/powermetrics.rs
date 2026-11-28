use crate::process_utils::command_output_timeout;
use crate::types::{DiskInfo, NetworkInfo};
use std::collections::HashMap;

/// Cumulative counters of one network interface, as `getifaddrs` reports
/// them: only the low 32 bits of the kernel's 64-bit counters, so the byte
/// counters wrap every 4 GiB. The 64-bit fields of sysctl `NET_RT_IFLIST2`
/// are no way around that: on macOS 27 an ordinary, unentitled process is
/// handed the same low 32 bits there.
#[derive(Clone, Copy, Debug, Default)]
pub struct IfCounters {
    pub bytes_in: u32,
    pub bytes_out: u32,
    pub packets_in: u32,
    pub packets_out: u32,
}

impl IfCounters {
    /// Bytes received and sent since `prev`, an earlier sample of the same
    /// interface.
    fn bytes_since(&self, prev: &IfCounters) -> (u64, u64) {
        // A byte counter that went backwards has normally wrapped, and
        // wrapping subtraction is then the true delta as long as the interface
        // moves less than 4 GiB per sample. The exception is an interface that
        // was re-created under the same name (VPN tunnel restart, dock
        // re-attach): the kernel starts its counters from zero again, and the
        // same subtraction would report up to 4 GiB that never moved. The
        // packet counters tell the two apart: they keep rising across a byte
        // wrap, so one of them going backwards means a re-created interface,
        // and the sample counts as zero. The price is one zero sample each
        // time a packet counter itself wraps, every 2^32 packets. Not caught:
        // a re-created interface that passes both of its old packet counts
        // within one sample.
        if self.packets_in < prev.packets_in || self.packets_out < prev.packets_out {
            return (0, 0);
        }
        (
            self.bytes_in.wrapping_sub(prev.bytes_in) as u64,
            self.bytes_out.wrapping_sub(prev.bytes_out) as u64,
        )
    }
}

pub type NetCounters = HashMap<String, IfCounters>; // iface → counters

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

/// Read cumulative network counters via getifaddrs (no subprocess).
pub fn read_net_counters() -> NetCounters {
    let mut result = NetCounters::new();
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
            let ib = data.ifi_ibytes;
            let ob = data.ifi_obytes;
            if ib > 0 || ob > 0 {
                let e = result.entry(name.into_owned()).or_default();
                e.bytes_in = e.bytes_in.max(ib);
                e.bytes_out = e.bytes_out.max(ob);
                e.packets_in = e.packets_in.max(data.ifi_ipackets);
                e.packets_out = e.packets_out.max(data.ifi_opackets);
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
        .filter_map(|(iface, c)| prev.get(iface).map(|p| c.bytes_since(p)))
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
        "nettop",
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

    /// Snapshot from (interface, bytes_in, bytes_out, packets_in, packets_out).
    fn snapshot(ifaces: &[(&str, u32, u32, u32, u32)]) -> NetCounters {
        ifaces
            .iter()
            .map(|&(name, bytes_in, bytes_out, packets_in, packets_out)| {
                let counters = IfCounters {
                    bytes_in,
                    bytes_out,
                    packets_in,
                    packets_out,
                };
                (name.to_string(), counters)
            })
            .collect()
    }

    #[test]
    fn compute_net_rates_aggregates_interfaces() {
        let prev = snapshot(&[("en0", 100, 200, 1, 2), ("en1", 50, 100, 1, 1)]);
        let cur = snapshot(&[("en0", 300, 500, 3, 5), ("en1", 100, 160, 2, 2)]);
        let rates = compute_net_rates(&prev, &cur, 2.0);
        assert_eq!(rates.bytes_in_per_sec, 125.0);
        assert_eq!(rates.bytes_out_per_sec, 180.0);
    }

    #[test]
    fn compute_net_rates_iface_filters_target_only() {
        let prev = snapshot(&[("en0", 100, 200, 1, 2), ("en1", 10, 20, 1, 1)]);
        let cur = snapshot(&[("en0", 160, 260, 2, 3), ("en1", 110, 220, 2, 3)]);
        let rates = compute_net_rates_iface(&prev, &cur, 2.0, "en0");
        assert_eq!(rates.bytes_in_per_sec, 30.0);
        assert_eq!(rates.bytes_out_per_sec, 30.0);
    }

    #[test]
    fn compute_net_rates_counts_across_32bit_wrap() {
        // en0's byte counters wrap in both directions between the two samples
        // while its packet counters keep rising; en1 does not wrap.
        let prev = snapshot(&[
            ("en0", u32::MAX - 999, u32::MAX - 1_999, 7_000, 9_000),
            ("en1", 10, 20, 1, 1),
        ]);
        let cur = snapshot(&[("en0", 1_000, 2_000, 7_002, 9_003), ("en1", 110, 220, 2, 3)]);
        let en0 = compute_net_rates_iface(&prev, &cur, 2.0, "en0");
        assert_eq!(en0.bytes_in_per_sec, 1_000.0);
        assert_eq!(en0.bytes_out_per_sec, 2_000.0);
        let all = compute_net_rates(&prev, &cur, 2.0);
        assert_eq!(all.bytes_in_per_sec, 1_050.0);
        assert_eq!(all.bytes_out_per_sec, 2_100.0);
    }

    #[test]
    fn compute_net_rates_skips_recreated_interface() {
        // utun5 was torn down and came back under the same name, so all its
        // counters restarted from zero. en0 kept running and is still counted.
        let prev = snapshot(&[
            ("utun5", 26_624, 55_296, 178, 274),
            ("en0", 1_000, 2_000, 10, 20),
        ]);
        let cur = snapshot(&[("utun5", 1_024, 2_048, 3, 5), ("en0", 1_500, 2_250, 11, 21)]);
        let utun5 = compute_net_rates_iface(&prev, &cur, 0.25, "utun5");
        assert_eq!(utun5.bytes_in_per_sec, 0.0);
        assert_eq!(utun5.bytes_out_per_sec, 0.0);
        let all = compute_net_rates(&prev, &cur, 0.25);
        assert_eq!(all.bytes_in_per_sec, 2_000.0);
        assert_eq!(all.bytes_out_per_sec, 1_000.0);

        // One packet counter going backwards is enough: here the new tunnel
        // has already sent more than the old one did, but received less.
        let cur = snapshot(&[("utun5", 1_024, 90_112, 3, 300)]);
        let utun5 = compute_net_rates_iface(&prev, &cur, 0.25, "utun5");
        assert_eq!(utun5.bytes_in_per_sec, 0.0);
        assert_eq!(utun5.bytes_out_per_sec, 0.0);
    }

    #[test]
    fn compute_net_rates_total_may_exceed_32_bits() {
        // 3 GB on each of two interfaces: the sum does not fit the counter type.
        let prev = snapshot(&[("en0", 1, 1, 1, 1), ("en1", 1, 1, 1, 1)]);
        let cur = snapshot(&[
            ("en0", 3_000_000_001, 1, 2_000_001, 1),
            ("en1", 3_000_000_001, 1, 2_000_001, 1),
        ]);
        let rates = compute_net_rates(&prev, &cur, 1.0);
        assert_eq!(rates.bytes_in_per_sec, 6_000_000_000.0);
        assert_eq!(rates.bytes_out_per_sec, 0.0);
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
