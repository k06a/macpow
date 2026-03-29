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

/// Read disk I/O rates from `iostat` (no sudo needed).
pub fn read_disk_rates() -> DiskInfo {
    let output = match std::process::Command::new("iostat")
        .args(["-d", "-c", "2", "-w", "1"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return DiskInfo::default(),
    };

    // iostat outputs two samples; take the last data line
    // Format: "  KB/t  tps  MB/s" per disk
    let lines: Vec<&str> = output.lines().collect();
    if let Some(last) = lines.last() {
        let cols: Vec<&str> = last.split_whitespace().collect();
        // cols: KB/t tps MB/s (for each disk, we take the first)
        if cols.len() >= 3 {
            let mb_s: f64 = cols[2].parse().unwrap_or(0.0);
            return DiskInfo {
                read_bytes_per_sec: mb_s * 1024.0 * 1024.0 * 0.5, // rough split
                write_bytes_per_sec: mb_s * 1024.0 * 1024.0 * 0.5,
            };
        }
    }
    DiskInfo::default()
}
