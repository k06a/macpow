use crate::cf_utils;
use crate::types::SocPower;
use anyhow::{bail, Result};
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::array::CFArrayRef;
use core_foundation_sys::base::CFTypeRef;
use core_foundation_sys::dictionary::{CFDictionaryRef, CFMutableDictionaryRef};
use core_foundation_sys::string::CFStringRef;
use std::collections::{BTreeMap, BTreeSet};
use std::marker::{PhantomData, PhantomPinned};
use std::ptr;
use std::time::Instant;

// ── opaque handle ────────────────────────────────────────────────────────────

#[repr(C)]
struct IOReportSubscriptionRaw {
    _data: [u8; 0],
    _pin: PhantomData<(*mut u8, PhantomPinned)>,
}
type IOReportSubscriptionRef = *mut IOReportSubscriptionRaw;

// ── FFI bindings to /usr/lib/libIOReport.dylib ──────────────────────────────

type CVoidRef = *const libc::c_void;

#[link(name = "IOReport", kind = "dylib")]
extern "C" {
    fn IOReportCopyChannelsInGroup(
        group: CFStringRef,
        subgroup: CFStringRef,
        c: u64,
        d: u64,
        e: u64,
    ) -> CFDictionaryRef;

    fn IOReportMergeChannels(a: CFDictionaryRef, b: CFDictionaryRef, nil: CFTypeRef);

    fn IOReportCreateSubscription(
        a: CVoidRef,
        desired: CFMutableDictionaryRef,
        subd: *mut CFMutableDictionaryRef,
        channel_id: u64,
        e: CFTypeRef,
    ) -> IOReportSubscriptionRef;

    fn IOReportCreateSamples(
        sub: IOReportSubscriptionRef,
        subd: CFMutableDictionaryRef,
        a: CFTypeRef,
    ) -> CFDictionaryRef;

    fn IOReportCreateSamplesDelta(
        prev: CFDictionaryRef,
        cur: CFDictionaryRef,
        a: CFTypeRef,
    ) -> CFDictionaryRef;

    fn IOReportChannelGetGroup(ch: CFDictionaryRef) -> CFStringRef;
    #[allow(dead_code)]
    fn IOReportChannelGetSubGroup(ch: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetChannelName(ch: CFDictionaryRef) -> CFStringRef;
    fn IOReportChannelGetUnitLabel(ch: CFDictionaryRef) -> CFStringRef;
    fn IOReportSimpleGetIntegerValue(ch: CFDictionaryRef, idx: i32) -> i64;

    #[allow(dead_code)]
    fn IOReportStateGetCount(ch: CFDictionaryRef) -> i32;
    #[allow(dead_code)]
    fn IOReportStateGetNameForIndex(ch: CFDictionaryRef, idx: i32) -> CFStringRef;
    #[allow(dead_code)]
    fn IOReportStateGetResidency(ch: CFDictionaryRef, idx: i32) -> i64;
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn cfstr_raw(s: &str) -> CFStringRef {
    let cf = CFString::new(s);
    let raw = cf.as_concrete_TypeRef();
    std::mem::forget(cf); // caller or IOReport takes ownership
    raw
}

/// Compute watts from an IOReport energy delta sample.
/// `val`      – integer energy from `IOReportSimpleGetIntegerValue`
/// `unit`     – unit label from the channel ("mJ", "uJ", "nJ")
/// `dt_ms`    – wall-clock duration in milliseconds between the two samples
fn energy_to_watts(val: i64, unit: &str, dt_ms: u64) -> f32 {
    let dt_s = dt_ms.max(1) as f32 / 1000.0;
    let per_sec = val as f32 / dt_s;
    match unit {
        "mJ" => per_sec / 1e3,
        "uJ" => per_sec / 1e6,
        "nJ" => per_sec / 1e9,
        _ => 0.0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum CpuKind {
    Efficiency,
    Performance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct CpuCoreKey {
    kind: CpuKind,
    cluster: usize,
    core: usize,
}

fn parse_usize_ascii(s: &str) -> Option<usize> {
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn parse_cpu_stats_core_key(name: &str) -> Option<CpuCoreKey> {
    let (kind, suffix) = if let Some(rest) = name.strip_prefix("ECPU") {
        (CpuKind::Efficiency, rest)
    } else if let Some(rest) = name.strip_prefix("MCPU") {
        (CpuKind::Efficiency, rest)
    } else if let Some(rest) = name.strip_prefix("PCPU") {
        (CpuKind::Performance, rest)
    } else {
        return None;
    };

    if !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let trimmed = if suffix.len() >= 3 && suffix.ends_with('0') {
        &suffix[..suffix.len() - 1]
    } else {
        suffix
    };

    let (cluster, core) = if trimmed.len() == 1 {
        (0, parse_usize_ascii(trimmed)?)
    } else {
        let (cluster, core) = trimmed.split_at(trimmed.len() - 1);
        (parse_usize_ascii(cluster)?, parse_usize_ascii(core)?)
    };

    Some(CpuCoreKey {
        kind,
        cluster,
        core,
    })
}

fn parse_acc_core_key(name: &str, prefix: &str, kind: CpuKind) -> Option<CpuCoreKey> {
    let rest = name.strip_prefix(prefix)?;
    let (cluster, core) = if let Some(core) = rest.strip_prefix("_CPU") {
        (0, parse_usize_ascii(core)?)
    } else {
        let (cluster, core) = rest.split_once("_CPU")?;
        (parse_usize_ascii(cluster)?, parse_usize_ascii(core)?)
    };

    Some(CpuCoreKey {
        kind,
        cluster,
        core,
    })
}

fn parse_acc_cluster_total(name: &str, prefix: &str) -> Option<usize> {
    let rest = name.strip_prefix(prefix)?;
    if rest == "_CPU" {
        Some(0)
    } else {
        parse_usize_ascii(rest.strip_suffix("_CPU")?)
    }
}

fn parse_legacy_ecpu_core_key(name: &str) -> Option<CpuCoreKey> {
    let rest = name.strip_prefix("MCPU")?;
    let (cluster, core) = rest.split_once('_')?;
    Some(CpuCoreKey {
        kind: CpuKind::Efficiency,
        cluster: parse_usize_ascii(cluster)?,
        core: parse_usize_ascii(core)?,
    })
}

fn parse_legacy_ecpu_cluster_total(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("MCPU")?;
    (!rest.contains('_'))
        .then(|| parse_usize_ascii(rest))
        .flatten()
}

fn parse_energy_core_key(name: &str) -> Option<CpuCoreKey> {
    parse_legacy_ecpu_core_key(name)
        .or_else(|| parse_acc_core_key(name, "EACC", CpuKind::Efficiency))
        .or_else(|| parse_acc_core_key(name, "PACC", CpuKind::Performance))
        .or_else(|| {
            let core = parse_usize_ascii(name.strip_prefix("PACC_")?)?;
            Some(CpuCoreKey {
                kind: CpuKind::Performance,
                cluster: 0,
                core,
            })
        })
}

fn parse_energy_cluster_total(name: &str) -> Option<(CpuKind, usize)> {
    parse_legacy_ecpu_cluster_total(name)
        .map(|cluster| (CpuKind::Efficiency, cluster))
        .or_else(|| {
            parse_acc_cluster_total(name, "EACC").map(|cluster| (CpuKind::Efficiency, cluster))
        })
        .or_else(|| {
            parse_acc_cluster_total(name, "PACC").map(|cluster| (CpuKind::Performance, cluster))
        })
        .or_else(|| (name == "PCPU").then_some((CpuKind::Performance, 0)))
}

fn default_core_name(key: CpuCoreKey, multi_cluster: bool) -> String {
    let prefix = match key.kind {
        CpuKind::Efficiency => "ECPU",
        CpuKind::Performance => "PCPU",
    };

    if multi_cluster {
        format!("{prefix}{}_{}", key.cluster, key.core)
    } else {
        format!("{prefix}{}", key.core)
    }
}

fn cluster_total_from_cores(
    keys: &[CpuCoreKey],
    power: &BTreeMap<CpuCoreKey, (String, f32)>,
) -> f32 {
    keys.iter()
        .map(|key| power.get(key).map(|(_, watts)| *watts).unwrap_or(0.0))
        .sum()
}

fn build_ordered_keys(
    stats_order: &[CpuCoreKey],
    power: &BTreeMap<CpuCoreKey, (String, f32)>,
    kind: CpuKind,
) -> Vec<CpuCoreKey> {
    if !stats_order.is_empty() {
        stats_order.to_vec()
    } else {
        power
            .keys()
            .copied()
            .filter(|key| key.kind == kind)
            .collect()
    }
}

// ── public sampler ───────────────────────────────────────────────────────────

// ── DVFS frequency tables from pmgr IORegistry ──────────────────────────────

/// Read voltage-states from AppleARMIODevice/pmgr and return frequency tables
/// for E-CPU, P-CPU, and GPU. Matches by entry count against IOReport state counts.
fn read_dvfs_freq_tables() -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    // Known approximate ranges:
    // E-CPU: 600-3500 MHz, P-CPU: 600-4600 MHz, GPU: 300-1800 MHz
    // The voltage-states blobs are pairs of (freq_u32_le, voltage_u32_le), 8 bytes each.
    // Freq scale: M1/M2/M3 = Hz (÷1M), M4/M5 = KHz (÷1K) for CPU; Hz (÷1M) for GPU.

    use crate::cf_utils;
    use crate::iokit_ffi::*;
    use core_foundation_sys::dictionary::CFDictionaryRef;

    let mut all_tables: Vec<(String, Vec<u32>)> = Vec::new();

    unsafe {
        let matching = IOServiceMatching(b"AppleARMIODevice\0".as_ptr() as *const i8);
        if matching.is_null() {
            return (vec![], vec![], vec![]);
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return (vec![], vec![], vec![]);
        }

        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }

            let mut name_buf = [0i8; 128];
            IORegistryEntryGetName(entry, name_buf.as_mut_ptr());
            let name = std::ffi::CStr::from_ptr(name_buf.as_ptr()).to_string_lossy();

            if name == "pmgr" {
                let mut props = std::ptr::null_mut();
                if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
                    && !props.is_null()
                {
                    let dict = props as CFDictionaryRef;

                    // Enumerate all voltage-states*-sram keys and plain voltage-states* keys
                    let count = core_foundation_sys::dictionary::CFDictionaryGetCount(dict);
                    let mut keys = vec![std::ptr::null(); count as usize];
                    let mut vals = vec![std::ptr::null(); count as usize];
                    core_foundation_sys::dictionary::CFDictionaryGetKeysAndValues(
                        dict,
                        keys.as_mut_ptr(),
                        vals.as_mut_ptr(),
                    );

                    for i in 0..count as usize {
                        let key_str =
                            cf_utils::cfstring_to_string(keys[i] as _).unwrap_or_default();
                        if !key_str.starts_with("voltage-states") {
                            continue;
                        }

                        let data_ref = vals[i] as core_foundation_sys::data::CFDataRef;
                        let len = core_foundation_sys::data::CFDataGetLength(data_ref);
                        if len < 8 {
                            continue;
                        }
                        let ptr = core_foundation_sys::data::CFDataGetBytePtr(data_ref);
                        let bytes = std::slice::from_raw_parts(ptr, len as usize);

                        let freqs: Vec<u32> = bytes
                            .chunks_exact(8)
                            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                            .filter(|&f| f > 0)
                            .collect();
                        if !freqs.is_empty() {
                            all_tables.push((key_str, freqs));
                        }
                    }

                    cf_utils::cf_release(props as _);
                }
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
    }

    // Convert raw frequencies to MHz
    // Heuristic: if max freq > 100_000, it's in Hz (÷1M); if > 1000, it's in KHz (÷1K)
    fn to_mhz(freqs: &[u32]) -> Vec<u32> {
        let max = *freqs.iter().max().unwrap_or(&0);
        if max > 100_000_000 {
            freqs.iter().map(|f| f / 1_000_000).collect()
        } else if max > 100_000 {
            freqs.iter().map(|f| f / 1_000).collect()
        } else {
            freqs.to_vec()
        }
    }

    // Find tables by preferring *-sram keys and matching reasonable freq ranges
    // E-CPU: max ~2000-3500 MHz, P-CPU: max ~3000-5000 MHz, GPU: max ~1000-2000 MHz
    let mut ecpu: Vec<u32> = vec![];
    let mut pcpu: Vec<u32> = vec![];
    let mut gpu: Vec<u32> = vec![];

    // Prefer -sram variants for CPU
    let sram_tables: Vec<_> = all_tables
        .iter()
        .filter(|(k, _)| k.ends_with("-sram"))
        .map(|(k, v)| (k.clone(), to_mhz(v)))
        .collect();

    let non_sram_tables: Vec<_> = all_tables
        .iter()
        .filter(|(k, _)| !k.ends_with("-sram") && !k.contains("sram"))
        .map(|(k, v)| (k.clone(), to_mhz(v)))
        .collect();

    // Find P-CPU: highest max freq among sram tables (typically 3000-6000+ MHz)
    if let Some((_, freqs)) = sram_tables
        .iter()
        .filter(|(_, v)| v.last().copied().unwrap_or(0) > 2000)
        .max_by_key(|(_, v)| v.last().copied().unwrap_or(0))
    {
        pcpu = freqs.clone();
    }

    // Find E-CPU: sram table with max freq less than P-CPU's max, at least 500 MHz
    let pcpu_max = pcpu.last().copied().unwrap_or(u32::MAX);
    if let Some((_, freqs)) = sram_tables
        .iter()
        .filter(|(_, v)| {
            let max = v.last().copied().unwrap_or(0);
            max > 500 && max < pcpu_max && v.len() > 2
        })
        .max_by_key(|(_, v)| v.last().copied().unwrap_or(0))
    {
        ecpu = freqs.clone();
    }

    // Find GPU: non-sram table with max freq in 300-5000 MHz range
    if let Some((_, freqs)) = non_sram_tables
        .iter()
        .filter(|(_, v)| {
            let max = v.last().copied().unwrap_or(0);
            max >= 300 && max <= 5000 && v.len() >= 3
        })
        .max_by_key(|(_, v)| v.len())
    {
        gpu = freqs.clone();
    }
    // Fallback: try sram tables for GPU
    if gpu.is_empty() {
        if let Some((_, freqs)) = sram_tables
            .iter()
            .filter(|(_, v)| {
                let max = v.last().copied().unwrap_or(0);
                max >= 300 && max <= 5000 && v.len() >= 3
            })
            .max_by_key(|(_, v)| v.len())
        {
            gpu = freqs.clone();
        }
    }

    (ecpu, pcpu, gpu)
}

pub struct IOReportSampler {
    subscription: IOReportSubscriptionRef,
    subscribed_channels: CFMutableDictionaryRef,
    pub ecpu_freqs: Vec<u32>, // MHz, indexed by DVFS state
    pub pcpu_freqs: Vec<u32>,
    pub gpu_freqs: Vec<u32>,
}

unsafe impl Send for IOReportSampler {}

impl IOReportSampler {
    /// Create a new sampler subscribing to Energy Model + CPU/GPU Stats.
    pub fn new() -> Result<Self> {
        unsafe {
            let channels: &[(&str, Option<&str>)] = &[
                ("Energy Model", None),
                ("CPU Stats", Some("CPU Core Performance States")),
                ("GPU Stats", Some("GPU Performance States")),
            ];

            let mut merged: CFDictionaryRef = ptr::null();

            for &(group, subgroup) in channels {
                let g = cfstr_raw(group);
                let sg = subgroup.map(cfstr_raw).unwrap_or(ptr::null());
                let ch = IOReportCopyChannelsInGroup(g, sg, 0, 0, 0);
                if ch.is_null() {
                    continue;
                }
                if merged.is_null() {
                    merged = ch;
                } else {
                    IOReportMergeChannels(merged, ch, ptr::null());
                    cf_utils::cf_release(ch as _);
                }
            }

            if merged.is_null() {
                bail!("IOReport: no channels found");
            }

            let desired = cf_utils::cfdict_mutable_copy(merged);
            cf_utils::cf_release(merged as _);

            let mut subd: CFMutableDictionaryRef = ptr::null_mut();
            let sub = IOReportCreateSubscription(ptr::null(), desired, &mut subd, 0, ptr::null());

            if sub.is_null() || subd.is_null() {
                bail!("IOReport: subscription failed");
            }

            // Read DVFS frequency tables from pmgr
            let (ecpu_freqs, pcpu_freqs, gpu_freqs) = read_dvfs_freq_tables();

            Ok(Self {
                subscription: sub,
                subscribed_channels: subd,
                ecpu_freqs,
                pcpu_freqs,
                gpu_freqs,
            })
        }
    }

    /// Take a single absolute sample snapshot.
    pub fn sample(&self) -> Result<Sample> {
        unsafe {
            let s = IOReportCreateSamples(self.subscription, self.subscribed_channels, ptr::null());
            if s.is_null() {
                bail!("IOReport: sampling failed");
            }
            Ok(Sample {
                inner: s,
                ts: Instant::now(),
            })
        }
    }

    /// Dump all subscribed channel names for diagnostics.
    pub fn dump_channels(&self) {
        if let Ok(sample) = self.sample() {
            unsafe {
                let channels_arr =
                    cf_utils::cfdict_get(sample.inner, "IOReportChannels") as CFArrayRef;
                if channels_arr.is_null() {
                    eprintln!("No IOReportChannels in sample");
                    return;
                }
                let n = cf_utils::cfarray_len(channels_arr);
                for i in 0..n {
                    let ch = cf_utils::cfarray_get(channels_arr, i) as CFDictionaryRef;
                    let group = cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch))
                        .unwrap_or_default();
                    let subgroup = cf_utils::cfstring_to_string(IOReportChannelGetSubGroup(ch))
                        .unwrap_or_default();
                    let name = cf_utils::cfstring_to_string(IOReportChannelGetChannelName(ch))
                        .unwrap_or_default();
                    let unit = cf_utils::cfstring_to_string(IOReportChannelGetUnitLabel(ch))
                        .unwrap_or_default();
                    println!("{:<20} {:<40} {:<30} {}", group, subgroup, name, unit);
                }
                println!("\n--- DVFS frequency tables ---");
                println!("E-CPU freqs (MHz): {:?}", self.ecpu_freqs);
                println!("P-CPU freqs (MHz): {:?}", self.pcpu_freqs);
                println!("GPU freqs (MHz):   {:?}", self.gpu_freqs);
            }
        }
    }

    /// Compute the delta between two samples and parse energy metrics.
    pub fn parse_power(&self, prev: &Sample, cur: &Sample) -> Result<SocPower> {
        let dt_ms = cur.ts.duration_since(prev.ts).as_millis() as u64;

        unsafe {
            let delta = IOReportCreateSamplesDelta(prev.inner, cur.inner, ptr::null());
            if delta.is_null() {
                bail!("IOReport: delta failed");
            }

            let channels_arr = cf_utils::cfdict_get(delta, "IOReportChannels") as CFArrayRef;
            if channels_arr.is_null() {
                cf_utils::cf_release(delta as _);
                bail!("IOReport: no channels in delta");
            }

            let mut soc = SocPower::default();

            let mut ecpu_order: Vec<CpuCoreKey> = Vec::new();
            let mut pcpu_order: Vec<CpuCoreKey> = Vec::new();
            let mut seen_stats_keys: BTreeSet<CpuCoreKey> = BTreeSet::new();

            let mut ecpu_power: BTreeMap<CpuCoreKey, (String, f32)> = BTreeMap::new();
            let mut pcpu_power: BTreeMap<CpuCoreKey, (String, f32)> = BTreeMap::new();
            let mut ecpu_totals: BTreeMap<usize, (String, f32)> = BTreeMap::new();
            let mut pcpu_totals: BTreeMap<usize, (String, f32)> = BTreeMap::new();

            // Frequency tracking: (state_index, residency_ns) per core
            let mut ecpu_freq_residency: Vec<(usize, i64)> = Vec::new();
            let mut pcpu_freq_residency: Vec<(usize, i64)> = Vec::new();
            let mut gpu_freq_residency: Vec<(usize, i64)> = Vec::new();
            // Idle+active residency for utilization calculation
            let mut gpu_idle_res: i64 = 0;
            let mut gpu_active_res: i64 = 0;

            let n = cf_utils::cfarray_len(channels_arr);

            for i in 0..n {
                let ch = cf_utils::cfarray_get(channels_arr, i) as CFDictionaryRef;
                let group =
                    cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch)).unwrap_or_default();

                let name = cf_utils::cfstring_to_string(IOReportChannelGetChannelName(ch))
                    .unwrap_or_default();

                if group == "CPU Stats" || group == "GPU Stats" {
                    let state_count = IOReportStateGetCount(ch);
                    let stats_key = parse_cpu_stats_core_key(&name);
                    if let Some(key) = stats_key {
                        if seen_stats_keys.insert(key) {
                            match key.kind {
                                CpuKind::Efficiency => ecpu_order.push(key),
                                CpuKind::Performance => pcpu_order.push(key),
                            }
                        }
                    }

                    let is_ecpu = matches!(
                        stats_key,
                        Some(CpuCoreKey {
                            kind: CpuKind::Efficiency,
                            ..
                        })
                    );
                    let is_pcpu = matches!(
                        stats_key,
                        Some(CpuCoreKey {
                            kind: CpuKind::Performance,
                            ..
                        })
                    );
                    let is_gpu = group == "GPU Stats";

                    // Skip IDLE/DOWN/OFF states, collect (index, residency) for active states
                    let offset = (0..state_count)
                        .position(|s| {
                            let sn =
                                cf_utils::cfstring_to_string(IOReportStateGetNameForIndex(ch, s))
                                    .unwrap_or_default();
                            sn != "IDLE" && sn != "DOWN" && sn != "OFF"
                        })
                        .unwrap_or(2) as i32;

                    // Collect idle residency (states before offset)
                    if is_gpu {
                        for s in 0..offset {
                            let r = IOReportStateGetResidency(ch, s);
                            if r > 0 {
                                gpu_idle_res += r;
                            }
                        }
                    }

                    for s in offset..state_count {
                        let residency = IOReportStateGetResidency(ch, s);
                        if residency <= 0 {
                            continue;
                        }
                        if is_gpu {
                            gpu_active_res += residency;
                        }
                        let idx = (s - offset) as usize;
                        let target = if is_gpu {
                            &mut gpu_freq_residency
                        } else if is_pcpu {
                            &mut pcpu_freq_residency
                        } else if is_ecpu {
                            &mut ecpu_freq_residency
                        } else {
                            continue;
                        };
                        target.push((idx, residency));
                    }
                    continue;
                }

                if group != "Energy Model" {
                    continue;
                }
                let unit = cf_utils::cfstring_to_string(IOReportChannelGetUnitLabel(ch))
                    .unwrap_or_default();
                let val = IOReportSimpleGetIntegerValue(ch, 0);
                let watts = energy_to_watts(val, &unit, dt_ms);

                match name.as_str() {
                    // Grand CPU total
                    n if n.ends_with("CPU Energy") => {
                        soc.cpu_w += watts;
                    }
                    "GPU Energy" => {
                        soc.gpu_w += watts;
                    }
                    n if n.starts_with("ANE") => {
                        soc.ane_w += watts;
                        soc.ane_parts.push((n.to_string(), watts));
                    }
                    n if n.starts_with("DRAM") => {
                        soc.dram_w += watts;
                    }
                    n if n.starts_with("GPU SRAM") => {
                        soc.gpu_sram_w += watts;
                    }
                    _ => {
                        if let Some(key) = parse_energy_core_key(&name) {
                            match key.kind {
                                CpuKind::Efficiency => {
                                    ecpu_power.insert(key, (name.clone(), watts));
                                }
                                CpuKind::Performance => {
                                    pcpu_power.insert(key, (name.clone(), watts));
                                }
                            }
                        } else if let Some((kind, cluster)) = parse_energy_cluster_total(&name) {
                            match kind {
                                CpuKind::Efficiency => {
                                    ecpu_totals.insert(cluster, (name.clone(), watts));
                                }
                                CpuKind::Performance => {
                                    pcpu_totals.insert(cluster, (name.clone(), watts));
                                }
                            }
                        }
                    }
                }
            }

            let ordered_ecpu_keys =
                build_ordered_keys(&ecpu_order, &ecpu_power, CpuKind::Efficiency);
            let ordered_pcpu_keys =
                build_ordered_keys(&pcpu_order, &pcpu_power, CpuKind::Performance);

            let ecpu_cluster_count = ordered_ecpu_keys
                .iter()
                .map(|key| key.cluster)
                .chain(ecpu_totals.keys().copied())
                .collect::<BTreeSet<_>>()
                .len();
            let pcpu_cluster_count = ordered_pcpu_keys
                .iter()
                .map(|key| key.cluster)
                .chain(pcpu_totals.keys().copied())
                .collect::<BTreeSet<_>>()
                .len();

            let mut ecpu_keys_by_cluster: BTreeMap<usize, Vec<CpuCoreKey>> = BTreeMap::new();
            for key in ordered_ecpu_keys {
                ecpu_keys_by_cluster
                    .entry(key.cluster)
                    .or_default()
                    .push(key);
            }

            for (cluster, keys) in ecpu_keys_by_cluster {
                let total_w = ecpu_totals
                    .get(&cluster)
                    .map(|(_, watts)| *watts)
                    .unwrap_or_else(|| cluster_total_from_cores(&keys, &ecpu_power));
                let name = ecpu_totals
                    .get(&cluster)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| format!("ECPU{cluster}"));
                let cores = keys
                    .into_iter()
                    .map(|key| {
                        let (name, watts) = ecpu_power.get(&key).cloned().unwrap_or_else(|| {
                            (default_core_name(key, ecpu_cluster_count > 1), 0.0)
                        });
                        crate::types::CpuCore { name, watts }
                    })
                    .collect();
                soc.ecpu_clusters.push(crate::types::CpuCluster {
                    name,
                    total_w,
                    cores,
                });
            }

            let p_cores: Vec<_> = ordered_pcpu_keys
                .into_iter()
                .map(|key| {
                    let (name, watts) = pcpu_power
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| (default_core_name(key, pcpu_cluster_count > 1), 0.0));
                    crate::types::CpuCore { name, watts }
                })
                .collect();
            soc.pcpu_cluster = crate::types::CpuCluster {
                name: "PCPU".to_string(),
                total_w: if pcpu_totals.is_empty() {
                    p_cores.iter().map(|core| core.watts).sum()
                } else {
                    pcpu_totals.values().map(|(_, watts)| *watts).sum()
                },
                cores: p_cores,
            };

            // Compute weighted average frequency in MHz using DVFS tables
            let calc_freq = |data: &[(usize, i64)], freqs: &[u32]| -> u32 {
                if freqs.is_empty() || data.is_empty() {
                    return 0;
                }
                let total_res: i64 = data.iter().map(|(_, r)| *r).sum();
                if total_res <= 0 {
                    return 0;
                }
                let weighted: f64 = data
                    .iter()
                    .map(|(idx, r)| {
                        let freq = freqs
                            .get(*idx)
                            .copied()
                            .unwrap_or(*freqs.last().unwrap_or(&0))
                            as f64;
                        freq * *r as f64
                    })
                    .sum();
                (weighted / total_res as f64).round() as u32
            };
            soc.ecpu_freq_mhz = calc_freq(&ecpu_freq_residency, &self.ecpu_freqs);
            soc.pcpu_freq_mhz = calc_freq(&pcpu_freq_residency, &self.pcpu_freqs);
            soc.gpu_freq_mhz = calc_freq(&gpu_freq_residency, &self.gpu_freqs);

            // GPU active residency % from IOReport (same method as asitop/mactop)
            let total_gpu_res = gpu_idle_res + gpu_active_res;
            if total_gpu_res > 0 {
                soc.gpu_util_device =
                    (gpu_active_res as f64 / total_gpu_res as f64 * 100.0).round() as u32;
            }

            cf_utils::cf_release(delta as _);
            soc.compute_total();
            Ok(soc)
        }
    }
}

/// An absolute IOReport snapshot at a point in time.
pub struct Sample {
    inner: CFDictionaryRef,
    ts: Instant,
}

unsafe impl Send for Sample {}

impl Drop for Sample {
    fn drop(&mut self) {
        unsafe {
            cf_utils::cf_release(self.inner as _);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modern_cpu_stats_names() {
        assert_eq!(
            parse_cpu_stats_core_key("ECPU030"),
            Some(CpuCoreKey {
                kind: CpuKind::Efficiency,
                cluster: 0,
                core: 3,
            })
        );
        assert_eq!(
            parse_cpu_stats_core_key("PCPU140"),
            Some(CpuCoreKey {
                kind: CpuKind::Performance,
                cluster: 1,
                core: 4,
            })
        );
    }

    #[test]
    fn parses_legacy_mcpu_cpu_stats_names() {
        assert_eq!(
            parse_cpu_stats_core_key("MCPU00"),
            Some(CpuCoreKey {
                kind: CpuKind::Efficiency,
                cluster: 0,
                core: 0,
            })
        );
        assert_eq!(
            parse_cpu_stats_core_key("MCPU15"),
            Some(CpuCoreKey {
                kind: CpuKind::Efficiency,
                cluster: 1,
                core: 5,
            })
        );
    }

    #[test]
    fn parses_legacy_and_modern_energy_names() {
        assert_eq!(
            parse_energy_core_key("MCPU1_2"),
            Some(CpuCoreKey {
                kind: CpuKind::Efficiency,
                cluster: 1,
                core: 2,
            })
        );
        assert_eq!(
            parse_energy_core_key("EACC_CPU3"),
            Some(CpuCoreKey {
                kind: CpuKind::Efficiency,
                cluster: 0,
                core: 3,
            })
        );
        assert_eq!(
            parse_energy_core_key("PACC1_CPU4"),
            Some(CpuCoreKey {
                kind: CpuKind::Performance,
                cluster: 1,
                core: 4,
            })
        );
        assert_eq!(
            parse_energy_cluster_total("EACC_CPU"),
            Some((CpuKind::Efficiency, 0))
        );
        assert_eq!(
            parse_energy_cluster_total("PACC1_CPU"),
            Some((CpuKind::Performance, 1))
        );
        assert_eq!(parse_energy_core_key("PACC1_CPU5_SRAM"), None);
    }
}
