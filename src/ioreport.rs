use crate::cf_utils;
use crate::iokit_ffi::{
    IOObjectRelease, IORegistryEntryCreateCFProperties, IORegistryEntryGetRegistryEntryID,
    IOServiceGetMatchingService, IOServiceMatching,
};
use crate::types::SocPower;
use anyhow::{bail, Result};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFDictionary, CFMutableDictionary};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::{CFArrayAppendValue, CFArrayCreateMutableCopy, CFArrayRef};
use core_foundation_sys::base::{CFGetTypeID, CFTypeRef};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef};
use core_foundation_sys::dictionary::{
    CFDictionaryRef, CFDictionarySetValue, CFMutableDictionaryRef,
};
use core_foundation_sys::string::CFStringRef;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::marker::{PhantomData, PhantomPinned};
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ── opaque handle ────────────────────────────────────────────────────────────

#[repr(C)]
struct IOReportSubscriptionRaw {
    _data: [u8; 0],
    _pin: PhantomData<(*mut u8, PhantomPinned)>,
}
type IOReportSubscriptionRef = *mut IOReportSubscriptionRaw;

// ── FFI bindings to /usr/lib/libIOReport.dylib ──────────────────────────────

type CVoidRef = *const libc::c_void;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

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

const CLPC_CPU_GROUP: &str = "CLPC";
const CLPC_CPU_CHANNEL: &str = "CPU Energy";

/// macOS 27 can stop updating the named CPU counter. Request the aggregate
/// report directly from the M5 Pro/Max CLPC driver when its mapping is known.
/// Report ID: https://github.com/vladkens/macmon/blob/7df49f5/src/clpc.rs
/// Channel format: https://github.com/vladkens/macmon/blob/7df49f5/src/sources.rs
unsafe fn channels_with_clpc_cpu(channels: CFDictionaryRef) -> Option<CFMutableDictionaryRef> {
    let mut version = [0u8; 32];
    let mut len = version.len();
    if libc::sysctlbyname(
        c"kern.osproductversion".as_ptr(),
        version.as_mut_ptr().cast(),
        &mut len,
        ptr::null_mut(),
        0,
    ) != 0
        || !version.starts_with(b"27.")
    {
        return None;
    }

    // Match the firmware driver, since the opaque report ID varies by driver.
    let service = IOServiceGetMatchingService(0, IOServiceMatching(c"AppleCLPC".as_ptr()));
    if service == 0 {
        return None;
    }
    let mut properties = ptr::null_mut();
    let mut driver_id = 0;
    let found = IORegistryEntryGetRegistryEntryID(service, &mut driver_id) == 0
        && IORegistryEntryCreateCFProperties(service, &mut properties, ptr::null(), 0) == 0;
    IOObjectRelease(service);
    let bundle = (found && !properties.is_null())
        .then(|| cf_utils::cfdict_get_string(properties, "CFBundleIdentifier"))
        .flatten();
    cf_utils::cf_release(properties as _);
    if bundle.as_deref() != Some("com.apple.driver.AppleT6050CLPC") {
        return None;
    }

    let key = CFString::new("IOReportChannels");
    let current = cf_utils::cfdict_get(channels, "IOReportChannels") as CFArrayRef;
    if current.is_null() {
        return None;
    }
    let selected = CFArrayCreateMutableCopy(ptr::null(), 0, current);
    if selected.is_null() {
        return None;
    }
    let with_clpc = cf_utils::cfdict_mutable_copy(channels);
    if with_clpc.is_null() {
        cf_utils::cf_release(selected as _);
        return None;
    }

    // A single integer in the power category, with a nanojoule unit label.
    let legend = CFArray::from_CFTypes(&[
        CFNumber::from(0x0000_0010_d3ab_60bb_i64).into_CFType(),
        CFNumber::from(1i64 | (2 << 16) | (1 << 32)).into_CFType(),
        CFString::new(CLPC_CPU_CHANNEL).into_CFType(),
    ]);
    let info = CFDictionary::from_CFType_pairs(&[(
        CFString::new("IOReportChannelUnit"),
        CFNumber::from((3i64 << 56) | (118i64 << 32)),
    )]);
    let channel = CFMutableDictionary::from_CFType_pairs(&[
        (
            CFString::new("DriverID"),
            CFNumber::from(driver_id as i64).into_CFType(),
        ),
        (
            CFString::new("DriverName"),
            CFString::new("AppleCLPC").into_CFType(),
        ),
        (
            CFString::new("IOReportGroupName"),
            CFString::new(CLPC_CPU_GROUP).into_CFType(),
        ),
        (
            CFString::new("IOReportSubGroupName"),
            CFString::new("Energy Counters").into_CFType(),
        ),
        (CFString::new("IOReportChannelInfo"), info.into_CFType()),
        (CFString::new("LegendChannel"), legend.into_CFType()),
    ]);
    CFArrayAppendValue(selected, channel.as_CFTypeRef());
    CFDictionarySetValue(with_clpc, key.as_CFTypeRef(), selected as _);
    cf_utils::cf_release(selected as _);
    Some(with_clpc)
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

/// Multi-die chips (M1/M2/M3 Ultra) prefix IOReport channel names with `DIE_{N}_`.
/// Single-die chips have no prefix. We strip it generically so all downstream
/// parsers work on the same base name format: `{TYPE}{cluster}_CPU{core}`.
/// Returns `(die_index, remaining_name)` — die 0 for single-die chips.
fn strip_die_prefix(name: &str) -> (usize, &str) {
    if let Some(rest) = name.strip_prefix("DIE_") {
        if let Some(idx) = rest.find('_') {
            if let Ok(die) = rest[..idx].parse::<usize>() {
                return (die, &rest[idx + 1..]);
            }
        }
    }
    (0, name)
}

/// Cluster offset per die — keeps cluster IDs unique across dies.
const CLUSTERS_PER_DIE: usize = 100;

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

    // Digit-suffix format: ECPU0, PCPU10 (single-die M1/M2/M3/M4)
    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
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

        return Some(CpuCoreKey {
            kind,
            cluster,
            core,
        });
    }

    // _CPU suffix format: ECPU_CPU0, PCPU1_CPU3 (multi-die Ultra after DIE_N_ stripped)
    parse_acc_core_key(name, "ECPU", CpuKind::Efficiency)
        .or_else(|| parse_acc_core_key(name, "PCPU", CpuKind::Performance))
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
            name_buf[127] = 0;
            if IORegistryEntryGetName(entry, name_buf.as_mut_ptr()) != 0 {
                IOObjectRelease(entry);
                continue;
            }
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
    cpu_breakdown: RefCell<CpuEnergyBreakdown>,
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
                // AppleARMBacklight publishes live backlight metrics here:
                //   playback backlight factors report → MIE factor, DPB factor
                //   brightness report                 → MilliNits value, MicroAmps value,
                //                                       UncalMilliNits value, UserBrightness value,
                //                                       RawBrightness value
                ("backlight report", None),
            ];

            let mut merged: CFDictionaryRef = ptr::null();

            for &(group, subgroup) in channels {
                let g_cf = CFString::new(group);
                let g = g_cf.as_concrete_TypeRef();
                let sg_cf = subgroup.map(CFString::new);
                let sg = sg_cf
                    .as_ref()
                    .map(|s| s.as_concrete_TypeRef())
                    .unwrap_or(ptr::null());
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
            if desired.is_null() {
                bail!("IOReport: failed to create mutable channels copy");
            }

            let subscribe = |channels| {
                let mut subd = ptr::null_mut();
                let sub =
                    IOReportCreateSubscription(ptr::null(), channels, &mut subd, 0, ptr::null());
                if sub.is_null() || subd.is_null() {
                    cf_utils::cf_release(sub as _);
                    cf_utils::cf_release(subd as _);
                    None
                } else {
                    Some((sub, subd))
                }
            };
            let subscription = channels_with_clpc_cpu(desired)
                .and_then(|channels| {
                    let subscription = subscribe(channels);
                    cf_utils::cf_release(channels as _);
                    subscription
                })
                .or_else(|| subscribe(desired));
            cf_utils::cf_release(desired as _);
            let Some((sub, subd)) = subscription else {
                bail!("IOReport: subscription failed");
            };

            // Read DVFS frequency tables from pmgr
            let (ecpu_freqs, pcpu_freqs, gpu_freqs) = read_dvfs_freq_tables();

            Ok(Self {
                subscription: sub,
                subscribed_channels: subd,
                cpu_breakdown: RefCell::new(CpuEnergyBreakdown::new()),
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

    /// Check the named Energy Model counters directly, independently of the
    /// source selected for the aggregate CPU power row.
    pub fn check_cpu_core_counters(&self) -> Result<()> {
        let first = self.sample()?;
        let mut checks: BTreeMap<_, _> = first
            .cpu_core_energy()
            .into_iter()
            .map(|(name, (unit, value))| {
                (
                    name,
                    (
                        unit,
                        CoreCounterCheck {
                            latest: value,
                            ..Default::default()
                        },
                    ),
                )
            })
            .collect();
        if checks.is_empty() {
            bail!("No per-core CPU Energy Model channels found");
        }
        while first.ts.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(100));
            let sample = self.sample()?;
            let readings = sample.cpu_core_energy();
            for (name, (unit, check)) in &mut checks {
                let reading = readings
                    .get(name)
                    .filter(|(u, _)| u == unit)
                    .map(|(_, v)| *v);
                check.observe(reading, sample.ts);
            }
        }

        println!(
            "Per-core Energy Model counters over {:.3} s:",
            first.ts.elapsed().as_secs_f64()
        );
        let mut advancing = 0;
        for (name, (unit, check)) in &checks {
            if check.invalid {
                println!("{name:<16}      N/A      counter reset or missing");
            } else if let Some((energy, dt_ms)) = check.measured_delta() {
                let watts = energy_to_watts(energy, unit, dt_ms);
                println!("{name:<16} {watts:>8.3} W    +{energy} {unit} over {dt_ms} ms");
                advancing += 1;
            } else if check.first_advance.is_some() {
                println!("{name:<16}      N/A      one update; no confirmed refresh interval");
                advancing += 1;
            } else {
                println!("{name:<16}      N/A      counter unchanged");
            }
        }
        println!("{advancing}/{} core counters advanced.", checks.len());
        Ok(())
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
                    let val = IOReportSimpleGetIntegerValue(ch, 0);
                    println!(
                        "{:<20} {:<40} {:<30} {:<8} {}",
                        group, subgroup, name, unit, val
                    );
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
            let mut clpc_cpu_w = None;

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
                    let (die, base_name) = strip_die_prefix(&name);
                    let die_offset = die * CLUSTERS_PER_DIE;
                    let stats_key = parse_cpu_stats_core_key(base_name).map(|mut k| {
                        k.cluster += die_offset;
                        k
                    });
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

                if group == CLPC_CPU_GROUP && name == CLPC_CPU_CHANNEL {
                    let val = IOReportSimpleGetIntegerValue(ch, 0);
                    if val >= 0 {
                        clpc_cpu_w = Some(energy_to_watts(val, "nJ", dt_ms));
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

                // Strip DIE_N_ prefix for CPU core channels; block channels use
                // per-die suffixes (e.g. "ISP0_0") instead, handled by starts_with.
                let (die, base_name) = strip_die_prefix(&name);
                let die_offset = die * CLUSTERS_PER_DIE;

                // Block power channels use bare names on single-die chips (e.g. "ISP")
                // but gain per-die suffixes on multi-die chips (e.g. "ISP0_0").
                // Using starts_with() handles both and any future suffix convention.
                match name.as_str() {
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
                    n if n.starts_with("ISP") => {
                        soc.isp_w += watts;
                    }
                    n if n.starts_with("DISPEXT") => {
                        soc.display_ext_w += watts;
                    }
                    n if n.starts_with("DISP") => {
                        soc.display_soc_w += watts;
                    }
                    n if n.starts_with("AVE") || n.starts_with("MSR") => {
                        soc.media_w += watts;
                    }
                    n if n.starts_with("PCIe Port") || n.starts_with("apciec") => {
                        soc.pcie_w += watts;
                    }
                    n if n.starts_with("AMCC")
                        || n.starts_with("DCS")
                        || n.starts_with("FAB")
                        || n.starts_with("AFR") =>
                    {
                        soc.fabric_w += watts;
                    }
                    _ => {
                        if let Some(mut key) = parse_energy_core_key(base_name) {
                            key.cluster += die_offset;
                            match key.kind {
                                CpuKind::Efficiency => {
                                    ecpu_power.insert(key, (name.clone(), watts));
                                }
                                CpuKind::Performance => {
                                    pcpu_power.insert(key, (name.clone(), watts));
                                }
                            }
                        } else if let Some((kind, cluster)) = parse_energy_cluster_total(base_name)
                        {
                            let cluster = cluster + die_offset;
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

            // On affected systems the old counters can stall or update in
            // bursts. Keep CLPC for the total, and use the named per-core
            // model only while its publication timestamps are advancing.
            if let Some(clpc_cpu_w) = clpc_cpu_w {
                soc.cpu_w = clpc_cpu_w;
                self.cpu_breakdown
                    .borrow_mut()
                    .apply(cur, &mut soc, &pcpu_totals);
            }
            cf_utils::cf_release(delta as _);
            soc.compute_total();
            Ok(soc)
        }
    }

    /// Read the current backlight snapshot from a sample, no delta needed.
    /// All channels are absolute readings (factors and raw measurements), so we
    /// can read directly from the sample dictionary without computing a delta.
    pub fn parse_backlight(&self, cur: &Sample) -> BacklightSample {
        let mut bl = BacklightSample::default();
        unsafe {
            let channels_arr = cf_utils::cfdict_get(cur.inner, "IOReportChannels") as CFArrayRef;
            if channels_arr.is_null() {
                return bl;
            }
            let n = cf_utils::cfarray_len(channels_arr);
            for i in 0..n {
                let ch = cf_utils::cfarray_get(channels_arr, i) as CFDictionaryRef;
                let group =
                    cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch)).unwrap_or_default();
                if group != "backlight report" {
                    continue;
                }
                let name = cf_utils::cfstring_to_string(IOReportChannelGetChannelName(ch))
                    .unwrap_or_default();
                let val = IOReportSimpleGetIntegerValue(ch, 0);
                match name.as_str() {
                    "DPB factor" => bl.dpb_factor_raw = val,
                    "MIE factor" => bl.mie_factor_raw = val,
                    "MicroAmps value" => bl.microamps = val,
                    "MilliNits value" => bl.millinits = val,
                    "UncalMilliNits value" => bl.uncal_millinits = val,
                    "UserBrightness value" => bl.user_brightness = val,
                    "RawBrightness value" => bl.raw_brightness = val,
                    _ => {}
                }
            }
        }
        bl
    }
}

/// Snapshot of `AppleARMBacklight` IOReport channels.
///
/// Both `*_factor_raw` fields are 16.16 fixed-point: divide by 65536 to get a
/// floating multiplier. `dpb_factor_raw == 65536` means "no HDR boost requested"
/// (factor 1.0); higher values indicate active EDR boost on HDR-capable panels.
#[derive(Clone, Copy, Debug, Default)]
pub struct BacklightSample {
    /// Display Power Boost factor — raw 16.16 fixed-point.
    /// `65536` = 1.0 (no boost), `> 65536` = HDR pipeline engaged.
    pub dpb_factor_raw: i64,
    /// Maximum Image Energy factor (panel-side throttling) — raw 16.16 fixed-point.
    pub mie_factor_raw: i64,
    /// Backlight LED current in microamperes.
    pub microamps: i64,
    /// Calibrated brightness in millinits (e.g. 381794 = 381.79 nits).
    pub millinits: i64,
    /// Uncalibrated brightness in millinits (raw nominal).
    pub uncal_millinits: i64,
    /// User-controlled brightness slider position (0 = off, 65536 = max).
    pub user_brightness: i64,
    /// Raw register value driving the backlight PWM/LED.
    pub raw_brightness: i64,
}

impl BacklightSample {
    /// `DPB factor` decoded as a float multiplier (1.0 = no boost).
    pub fn dpb_factor(&self) -> f32 {
        self.dpb_factor_raw as f32 / 65536.0
    }
}

impl Drop for IOReportSampler {
    fn drop(&mut self) {
        unsafe {
            cf_utils::cf_release(self.subscription as _);
            cf_utils::cf_release(self.subscribed_channels as _);
        }
    }
}

/// An absolute IOReport snapshot at a point in time.
pub struct Sample {
    inner: CFDictionaryRef,
    ts: Instant,
}

// Allow a few missed 250 ms publications before dropping the last reading.
const CPU_ENERGY_MAX_AGE: f64 = 2.0;

#[derive(Clone, Copy)]
struct CpuEnergyReading {
    ticks: u64,
    millijoules: i64,
}

#[derive(Default)]
struct CpuEnergyCounter {
    previous: Option<CpuEnergyReading>,
    watts: Option<f32>,
}

impl CpuEnergyCounter {
    fn observe(&mut self, reading: CpuEnergyReading, now: u64, seconds_per_tick: f64) {
        if now
            .checked_sub(reading.ticks)
            .is_none_or(|age| age as f64 * seconds_per_tick > CPU_ENERGY_MAX_AGE)
        {
            *self = Self::default();
            return;
        }
        if let Some(previous) = self.previous {
            if reading.ticks == previous.ticks && reading.millijoules == previous.millijoules {
                return;
            }
            let dt = reading.ticks.saturating_sub(previous.ticks) as f64 * seconds_per_tick;
            self.watts = if dt > 0.0
                && dt <= CPU_ENERGY_MAX_AGE
                && reading.millijoules >= previous.millijoules
            {
                Some(((reading.millijoules - previous.millijoules) as f64 / (dt * 1000.0)) as f32)
            } else {
                None
            };
        }
        self.previous = Some(reading);
    }
}

struct CpuEnergyBreakdown {
    counters: BTreeMap<String, CpuEnergyCounter>,
}

impl CpuEnergyBreakdown {
    fn new() -> Self {
        Self {
            counters: BTreeMap::new(),
        }
    }

    fn apply(
        &mut self,
        sample: &Sample,
        soc: &mut SocPower,
        pcpu_totals: &BTreeMap<usize, (String, f32)>,
    ) {
        let readings = sample.published_cpu_energy();
        self.counters.retain(|name, _| readings.contains_key(name));
        static SECONDS_PER_TICK: OnceLock<Option<f64>> = OnceLock::new();
        let seconds_per_tick = SECONDS_PER_TICK.get_or_init(|| {
            let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
            let valid = unsafe { mach_timebase_info(&mut timebase) } == 0
                && timebase.numer != 0
                && timebase.denom != 0;
            valid.then(|| timebase.numer as f64 / timebase.denom as f64 * 1e-9)
        });
        if let Some(seconds_per_tick) = *seconds_per_tick {
            let now = unsafe { mach_absolute_time() };
            for (name, reading) in readings {
                self.counters
                    .entry(name)
                    .or_default()
                    .observe(reading, now, seconds_per_tick);
            }
        }
        let watts = |name: &str| self.counters.get(name).and_then(|counter| counter.watts);
        let mut complete = !soc.ecpu_clusters.is_empty() && !soc.pcpu_cluster.cores.is_empty();
        let clusters = soc
            .ecpu_clusters
            .iter_mut()
            .chain(std::iter::once(&mut soc.pcpu_cluster));
        for cluster in clusters {
            for core in &mut cluster.cores {
                let power = watts(&core.name);
                complete &= power.is_some();
                core.watts = power.unwrap_or_default();
            }
        }
        soc.cpu_breakdown_unavailable = !complete;
        let pcpu_w = pcpu_totals
            .values()
            .map(|(name, _)| watts(name))
            .sum::<Option<f32>>();
        for cluster in &mut soc.ecpu_clusters {
            cluster.total_w = if complete {
                watts(&cluster.name)
                    .unwrap_or_else(|| cluster.cores.iter().map(|core| core.watts).sum())
            } else {
                0.0
            };
        }
        let performance = &mut soc.pcpu_cluster;
        performance.total_w = if !complete {
            0.0
        } else if let Some(cluster_power) = pcpu_w.filter(|_| !pcpu_totals.is_empty()) {
            cluster_power
        } else {
            performance.cores.iter().map(|core| core.watts).sum()
        };
        if !complete {
            for core in soc
                .ecpu_clusters
                .iter_mut()
                .chain(std::iter::once(performance))
                .flat_map(|cluster| &mut cluster.cores)
            {
                core.watts = 0.0;
            }
        }
    }
}

#[derive(Default)]
struct CoreCounterCheck {
    latest: i64,
    first_advance: Option<(i64, Instant)>,
    last_advance: Option<Instant>,
    invalid: bool,
}

impl CoreCounterCheck {
    fn observe(&mut self, value: Option<i64>, time: Instant) {
        let Some(value) = value.filter(|&v| v >= self.latest) else {
            self.invalid = true;
            return;
        };
        if value > self.latest {
            if self.first_advance.is_none() {
                // The first jump can cover time before we started sampling.
                self.first_advance = Some((value, time));
            } else {
                self.last_advance = Some(time);
            }
            self.latest = value;
        }
    }

    fn measured_delta(&self) -> Option<(i64, u64)> {
        let (start, time) = self.first_advance?;
        let ms = self.last_advance?.duration_since(time).as_millis() as u64;
        (!self.invalid && ms > 0).then_some((self.latest - start, ms))
    }
}

impl Sample {
    fn published_cpu_energy(&self) -> BTreeMap<String, CpuEnergyReading> {
        unsafe {
            let channels = cf_utils::cfdict_get(self.inner, "IOReportChannels") as CFArrayRef;
            (0..cf_utils::cfarray_len(channels))
                .filter_map(|i| {
                    let channel = cf_utils::cfarray_get(channels, i) as CFDictionaryRef;
                    let group = cf_utils::cfstring_to_string(IOReportChannelGetGroup(channel))?;
                    if group != "Energy Model" {
                        return None;
                    }
                    let name =
                        cf_utils::cfstring_to_string(IOReportChannelGetChannelName(channel))?;
                    let base_name = strip_die_prefix(&name).1;
                    if parse_energy_core_key(base_name).is_none()
                        && parse_energy_cluster_total(base_name).is_none()
                    {
                        return None;
                    }
                    let unit = cf_utils::cfstring_to_string(IOReportChannelGetUnitLabel(channel))?;
                    if unit != "mJ" {
                        return None;
                    }
                    let data = cf_utils::cfdict_get(channel, "RawElements") as CFDataRef;
                    // IOReportElement: one 64-byte simple report, with the
                    // Mach publication timestamp and value following its header.
                    // See DriverKit/IOKernelReportStructs.h in Apple's XNU source.
                    if data.is_null()
                        || CFGetTypeID(data as _) != CFDataGetTypeID()
                        || CFDataGetLength(data) != 64
                    {
                        return None;
                    }
                    let ptr = CFDataGetBytePtr(data);
                    if ptr.is_null() {
                        return None;
                    }
                    let bytes = std::slice::from_raw_parts(ptr, 64);
                    let ticks = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
                    let millijoules = IOReportSimpleGetIntegerValue(channel, 0);
                    (ticks != 0 && millijoules >= 0)
                        .then_some((name, CpuEnergyReading { ticks, millijoules }))
                })
                .collect()
        }
    }

    fn cpu_core_energy(&self) -> BTreeMap<String, (String, i64)> {
        unsafe {
            let channels = cf_utils::cfdict_get(self.inner, "IOReportChannels") as CFArrayRef;
            (0..cf_utils::cfarray_len(channels))
                .filter_map(|i| {
                    let channel = cf_utils::cfarray_get(channels, i) as CFDictionaryRef;
                    let group = cf_utils::cfstring_to_string(IOReportChannelGetGroup(channel))?;
                    if group != "Energy Model" {
                        return None;
                    }
                    let name =
                        cf_utils::cfstring_to_string(IOReportChannelGetChannelName(channel))?;
                    parse_energy_core_key(strip_die_prefix(&name).1)?;
                    let unit = cf_utils::cfstring_to_string(IOReportChannelGetUnitLabel(channel))?;
                    let reading = IOReportSimpleGetIntegerValue(channel, 0);
                    (matches!(unit.as_str(), "mJ" | "uJ" | "nJ") && reading >= 0)
                        .then_some((name, (unit, reading)))
                })
                .collect()
        }
    }
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
    fn published_energy_uses_actual_sample_time_and_expires() {
        let mut counter = CpuEnergyCounter::default();
        let seconds_per_tick = 0.001;
        let reading = |ticks, millijoules| CpuEnergyReading { ticks, millijoules };
        counter.observe(reading(1_000, 3_000), 1_030, seconds_per_tick);
        assert_eq!(counter.watts, None);
        counter.observe(reading(1_250, 3_500), 1_400, seconds_per_tick);
        assert_eq!(counter.watts, Some(2.0));
        counter.observe(reading(1_250, 3_500), 1_430, seconds_per_tick);
        assert_eq!(counter.watts, Some(2.0));
        counter.observe(reading(1_250, 3_500), 3_251, seconds_per_tick);
        assert_eq!(counter.watts, None);
        counter.observe(reading(3_300, 100_000), 3_300, seconds_per_tick);
        assert_eq!(counter.watts, None);
        counter.observe(reading(3_550, 100_000), 3_560, seconds_per_tick);
        assert_eq!(counter.watts, Some(0.0));
    }

    #[test]
    fn per_core_diagnostic_excludes_the_initial_counter_jump() {
        let start = Instant::now();
        let mut check = CoreCounterCheck {
            latest: 1_000,
            ..Default::default()
        };
        check.observe(Some(90_000), start);
        assert_eq!(check.measured_delta(), None);
        check.observe(Some(90_000), start + Duration::from_millis(100));
        check.observe(Some(90_600), start + Duration::from_millis(500));
        assert_eq!(check.measured_delta(), Some((600, 500)));
        check.observe(Some(0), start + Duration::from_secs(1));
        assert_eq!(check.measured_delta(), None);
    }

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

    #[test]
    fn converts_energy_delta_to_watts_for_known_units() {
        let mj = energy_to_watts(1000, "mJ", 1000);
        let uj = energy_to_watts(1_000_000, "uJ", 1000);
        let nj = energy_to_watts(1_000_000_000, "nJ", 1000);
        assert!((mj - 1.0).abs() < 1e-6);
        assert!((uj - 1.0).abs() < 1e-6);
        assert!((nj - 1.0).abs() < 1e-6);
    }

    #[test]
    fn converts_energy_delta_unknown_unit_to_zero() {
        assert_eq!(energy_to_watts(123, "J", 1000), 0.0);
    }
}
