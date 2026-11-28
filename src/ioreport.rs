use crate::cf_utils;
use crate::clpc::{self, ClpcKind, ClpcReport};
use crate::topology::{CpuTopology, Tier};
use crate::types::SocPower;
use anyhow::{bail, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFMutableDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::array::{
    kCFTypeArrayCallBacks, CFArrayAppendValue, CFArrayCreateMutable, CFArrayCreateMutableCopy,
    CFArrayRef,
};
use core_foundation_sys::base::{CFGetTypeID, CFTypeRef};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef};
use core_foundation_sys::dictionary::{
    CFDictionaryRef, CFDictionarySetValue, CFMutableDictionaryRef,
};
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
    fn IOReportChannelGetChannelID(ch: CFDictionaryRef) -> u64;
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
    } else {
        (CpuKind::Performance, name.strip_prefix("PCPU")?)
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

/// One CPU DVFS operating point: frequency and core rail voltage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DvfsState {
    pub mhz: u32,
    /// Core (logic) rail voltage in mV; 0 when the device tree doesn't say.
    pub mv: u32,
}

impl DvfsState {
    /// Relative dynamic power of running in this state (∝ C·V²·f). Falls back
    /// to plain frequency when the voltage is unknown.
    fn dyn_weight(&self) -> f64 {
        let f = self.mhz as f64 / 1000.0;
        if self.mv > 0 {
            let v = self.mv as f64 / 1000.0;
            v * v * f
        } else {
            f
        }
    }
}

#[derive(Clone, Debug, Default)]
struct DvfsTables {
    ecpu: Vec<DvfsState>,
    pcpu: Vec<DvfsState>,
    gpu: Vec<u32>,
}

/// Convert raw frequencies to MHz.
/// Heuristic: if max freq > 100_000_000 it's in Hz (÷1M); if > 100_000 it's in kHz (÷1K).
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

/// Core-rail voltages for the CPU table stored under `sram_key`
/// (`voltage-statesN-sram`), one per entry whose frequency is non-zero.
///
/// The `-sram` table carries the frequencies but the SRAM rail voltage; the
/// logic rail voltage that drives dynamic power sits in the same-numbered plain
/// table (`voltage-statesN`). Falls back to the SRAM voltage, then to "unknown".
fn core_voltages(raw: &[(String, Vec<(u32, u32)>)], sram_key: &str) -> Vec<u32> {
    let Some((_, sram)) = raw.iter().find(|(k, _)| k == sram_key) else {
        return Vec::new();
    };
    let plausible = |mv: u32| (300..=1600).contains(&mv);
    let pick = |table: &[(u32, u32)]| -> Option<Vec<u32>> {
        (table.len() == sram.len()).then(|| {
            sram.iter()
                .zip(table)
                .filter(|((f, _), _)| *f > 0)
                .map(|(_, (_, mv))| *mv)
                .collect::<Vec<u32>>()
        })
    };
    let core = sram_key
        .strip_suffix("-sram")
        .and_then(|k| raw.iter().find(|(key, _)| key == k))
        .and_then(|(_, t)| pick(t))
        .filter(|v| !v.is_empty() && v.iter().all(|mv| plausible(*mv)));
    core.or_else(|| pick(sram).filter(|v| !v.is_empty() && v.iter().all(|mv| plausible(*mv))))
        .unwrap_or_default()
}

fn zip_states(freqs: &[u32], mvs: &[u32]) -> Vec<DvfsState> {
    freqs
        .iter()
        .enumerate()
        .map(|(i, &mhz)| DvfsState {
            mhz,
            mv: if mvs.len() == freqs.len() { mvs[i] } else { 0 },
        })
        .collect()
}

/// A little-endian `u32` device-tree property (`perf-state-count` and the like).
fn dt_u32(bytes: &[u8]) -> Option<u32> {
    let b = bytes.get(..4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Frequencies (MHz) of the GPU's performance states, from the properties of
/// the device-tree `sgx` node: the table the GPU driver itself runs on.
///
/// `perf-states` holds one table per voltage bin, each of `state_count`
/// (`perf-state-count`) pairs of frequency in Hz and voltage in mV. The bins
/// differ in voltage only, so the first table is read. Its 0 Hz entry is the
/// powered-off state, which IOReport lists as `OFF` rather than as a P-state.
///
/// `None` when the blob isn't a whole number of such tables, doesn't hold the
/// `active_states` (`gpu-num-perf-states`) the node announces, or doesn't read
/// as a GPU's frequencies: better to fall back to guessing among the pmgr
/// tables than to trust a misread one.
fn gpu_freqs_from_sgx(
    perf_states: &[u8],
    state_count: usize,
    active_states: Option<usize>,
) -> Option<Vec<u32>> {
    let table_len = state_count.checked_mul(8).filter(|&len| len > 0)?;
    let mut tables = perf_states.chunks_exact(table_len);
    let first = tables.next()?;
    if !tables.remainder().is_empty() {
        return None;
    }
    // Each 8-byte pair starts with its frequency.
    let hz: Vec<u32> = first
        .chunks(8)
        .filter_map(dt_u32)
        .filter(|&f| f > 0)
        .collect();
    let as_announced = active_states.unwrap_or(hz.len()) == hz.len();
    // A node needn't announce its states, and the length alone proves little:
    // a blob in another layout can have it too. So the numbers must read as
    // GPU states as well, rising from a whole MHz to a top inside the range
    // the pmgr guess accepts.
    let mhz = to_mhz(&hz);
    let rising = mhz.first().is_some_and(|&low| low > 0) && mhz.windows(2).all(|w| w[0] < w[1]);
    let top_plausible = mhz.last().is_some_and(|top| (300..=5000).contains(top));
    (as_announced && rising && top_plausible).then_some(mhz)
}

/// Pick the E-CPU, P-CPU and GPU tables out of every pmgr `voltage-states*`
/// blob. `sgx_gpu` is the GPU table of the `sgx` node, where it has one.
fn select_dvfs_tables(raw: &[(String, Vec<(u32, u32)>)], sgx_gpu: Option<Vec<u32>>) -> DvfsTables {
    // Frequency lists with zero entries dropped, converted to MHz.
    let all_tables: Vec<(String, Vec<u32>)> = raw
        .iter()
        .map(|(k, pairs)| {
            let freqs: Vec<u32> = pairs.iter().map(|(f, _)| *f).filter(|&f| f > 0).collect();
            (k.clone(), freqs)
        })
        .filter(|(_, freqs)| !freqs.is_empty())
        .collect();

    // Find tables by preferring *-sram keys and matching reasonable freq ranges
    // E-CPU: max ~2000-3500 MHz, P-CPU: max ~3000-5000 MHz, GPU: max ~1000-2000 MHz
    let mut out = DvfsTables::default();

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
    let mut pcpu_freqs: Vec<u32> = vec![];
    if let Some((key, freqs)) = sram_tables
        .iter()
        .filter(|(_, v)| v.last().copied().unwrap_or(0) > 2000)
        .max_by_key(|(_, v)| v.last().copied().unwrap_or(0))
    {
        pcpu_freqs = freqs.clone();
        out.pcpu = zip_states(freqs, &core_voltages(raw, key));
    }

    // Find E-CPU: sram table with max freq less than P-CPU's max, at least 500 MHz
    let pcpu_max = pcpu_freqs.last().copied().unwrap_or(u32::MAX);
    if let Some((key, freqs)) = sram_tables
        .iter()
        .filter(|(_, v)| {
            let max = v.last().copied().unwrap_or(0);
            max > 500 && max < pcpu_max && v.len() > 2
        })
        .max_by_key(|(_, v)| v.last().copied().unwrap_or(0))
    {
        out.ecpu = zip_states(freqs, &core_voltages(raw, key));
    }

    // GPU: the driver's own table if the device tree gave one. Otherwise guess:
    // the longest non-sram table with max freq in 300-5000 MHz range. (On an
    // M5 Max that guess lands on `voltage-states8`, 26 states of something
    // else, and an idle GPU read 732 MHz instead of 338.)
    if let Some(freqs) = sgx_gpu {
        out.gpu = freqs;
    } else if let Some((_, freqs)) = non_sram_tables
        .iter()
        .filter(|(_, v)| {
            let max = v.last().copied().unwrap_or(0);
            (300..=5000).contains(&max) && v.len() >= 3
        })
        .max_by_key(|(_, v)| v.len())
    {
        out.gpu = freqs.clone();
    }
    // Fallback: try sram tables for GPU
    if out.gpu.is_empty() {
        if let Some((_, freqs)) = sram_tables
            .iter()
            .filter(|(_, v)| {
                let max = v.last().copied().unwrap_or(0);
                (300..=5000).contains(&max) && v.len() >= 3
            })
            .max_by_key(|(_, v)| v.len())
        {
            out.gpu = freqs.clone();
        }
    }

    out
}

/// Read voltage-states from AppleARMIODevice/pmgr, and the GPU's performance
/// states from AppleARMIODevice/sgx, and return the DVFS tables for E-CPU,
/// P-CPU, and GPU.
fn read_dvfs_tables() -> DvfsTables {
    // Known approximate ranges:
    // E-CPU: 600-3500 MHz, P-CPU: 600-4600 MHz, GPU: 300-1800 MHz
    // The voltage-states blobs are pairs of (freq_u32_le, voltage_u32_le), 8 bytes each.
    // Freq scale: M1/M2/M3 = Hz (÷1M), M4/M5 = KHz (÷1K) for CPU; Hz (÷1M) for GPU.

    use crate::iokit_ffi::*;

    let mut raw: Vec<(String, Vec<(u32, u32)>)> = Vec::new();
    let mut sgx_gpu: Option<Vec<u32>> = None;

    unsafe {
        let matching = IOServiceMatching(b"AppleARMIODevice\0".as_ptr() as *const i8);
        if matching.is_null() {
            return DvfsTables::default();
        }
        let mut iter: u32 = 0;
        if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
            return DvfsTables::default();
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
                        if vals[i].is_null() || CFGetTypeID(vals[i]) != CFDataGetTypeID() {
                            continue;
                        }

                        let data_ref = vals[i] as CFDataRef;
                        let len = CFDataGetLength(data_ref);
                        if len < 8 {
                            continue;
                        }
                        let ptr = CFDataGetBytePtr(data_ref);
                        let bytes = std::slice::from_raw_parts(ptr, len as usize);

                        // (chunks + length filter rather than chunks_exact:
                        // clippy wants `as_chunks` there, which needs Rust 1.88.)
                        let pairs: Vec<(u32, u32)> = bytes
                            .chunks(8)
                            .filter(|c| c.len() == 8)
                            .map(|c| {
                                (
                                    u32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                                    u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                                )
                            })
                            .collect();
                        if pairs.iter().any(|(f, _)| *f > 0) {
                            raw.push((key_str, pairs));
                        }
                    }

                    cf_utils::cf_release(props as _);
                }
            } else if name == "sgx" && sgx_gpu.is_none() {
                // (One node is expected. Should there be another, it mustn't
                // replace a table already read with one that doesn't parse.)
                let mut props = std::ptr::null_mut();
                if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
                    && !props.is_null()
                {
                    // SAFETY: `props` is the live dictionary created just
                    // above; it is released only after the last read from it.
                    let dict = props as CFDictionaryRef;
                    let count = |key: &str| {
                        let n = dt_u32(&cfdict_bytes(dict, key)?)?;
                        Some(n as usize)
                    };
                    if let (Some(blob), Some(state_count)) =
                        (cfdict_bytes(dict, "perf-states"), count("perf-state-count"))
                    {
                        sgx_gpu =
                            gpu_freqs_from_sgx(&blob, state_count, count("gpu-num-perf-states"));
                    }
                    cf_utils::cf_release(props as _);
                }
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
    }

    select_dvfs_tables(&raw, sgx_gpu)
}

/// A copy of the bytes of the `CFData` stored under `key`; `None` if the key
/// is missing or holds something else.
///
/// `dict` must be a valid `CFDictionary`.
unsafe fn cfdict_bytes(dict: CFDictionaryRef, key: &str) -> Option<Vec<u8>> {
    let val = cf_utils::cfdict_get(dict, key);
    if val.is_null() || CFGetTypeID(val) != CFDataGetTypeID() {
        return None;
    }
    let data = val as CFDataRef;
    let len = usize::try_from(CFDataGetLength(data)).ok()?;
    let ptr = CFDataGetBytePtr(data);
    if len == 0 || ptr.is_null() {
        return None;
    }
    // SAFETY: `val` was just checked to be a CFData, whose byte pointer is
    // valid for its length while the dictionary holding it is alive. The
    // bytes are copied out before returning, so nothing outlives `dict`.
    Some(std::slice::from_raw_parts(ptr, len).to_vec())
}

// ── live power sources for systems where the Energy Model is gated ───────────

/// Cluster identity shared by Energy Model names, PMP histogram names and the
/// CLPC cluster order: tier plus cluster number within that tier.
type ClusterKey = (CpuKind, usize);

/// Seconds of "cores are busy but the CPU energy counters don't move" after
/// which the Energy Model is considered gated. A live counter moves by several
/// mJ per sample even at idle, so a full second of zeros can't be idle.
const GATE_AFTER_S: f32 = 1.0;

/// CPU power above this is not a reading but a catch-up: minutes of energy
/// published at once by an entitled sampler and divided by one sample interval.
const SPIKE_LIMIT_W: f32 = 1000.0;

/// Silence (with active cores) after which the counters' next movement is
/// taken for a catch-up rather than a reading. A live counter on an idle Mac
/// can skip a 100 ms sample or two (a few mW is under 1 mJ per sample), but
/// not half a second.
const CATCH_UP_AFTER_S: f32 = 0.5;

/// Consecutive samples with moving counters after which a gated Energy Model
/// is trusted again (when no CLPC source has taken over).
const UNGATE_AFTER: u32 = 8;

/// Energy (J) the PMP histograms must have seen above their lowest bucket in
/// one interval to prove, against a silent mJ counter, that the counter is
/// frozen rather than merely below its resolution.
const PMP_CONCLUSIVE_J: f32 = 0.05;

/// Share of a cluster's energy that the Energy Model attributes to the cores
/// themselves (the rest is the cluster's shared logic and SRAM). Measured
/// 0.61–0.73 on an M5 Max; replaced by the machine's own ratio once known.
const DEFAULT_CORE_SHARE: f32 = 0.68;

/// What one core did during a sample interval.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CoreActivity {
    /// Σ over active DVFS states of residency fraction × V²·f — relative
    /// dynamic energy. Cores of one cluster share V and f at any instant, so
    /// this is "time active, weighted by how hard the cluster ran meanwhile".
    dyn_weight: f64,
    /// Fraction of the interval spent in any active state.
    active: f64,
}

/// Energy rates read from CLPC over one interval, in watts.
#[derive(Clone, Debug, Default, PartialEq)]
struct ClpcSample {
    cpu_w: Option<f32>,
    gpu_w: Option<f32>,
    ane_w: Option<f32>,
    dram_w: Option<f32>,
    dcs_w: Option<f32>,
    package_w: Option<f32>,
    /// Per logical cluster, in kernel cluster order.
    clusters_w: Option<Vec<f32>>,
}

/// Everything one IOReport delta yields, before a power source is chosen.
#[derive(Debug, Default)]
pub struct RawSoc {
    dt_s: f32,
    /// The Energy Model view: exact on systems where the counters are live,
    /// zeros (or a catch-up spike) where they are gated.
    em: SocPower,
    /// Cluster number and core keys behind each entry of `em.ecpu_clusters`.
    e_clusters: Vec<(usize, Vec<CpuCoreKey>)>,
    /// Core keys behind `em.pcpu_cluster.cores`.
    p_cores: Vec<CpuCoreKey>,
    /// A PMGR CPU energy counter moved during this interval.
    em_cpu_advanced: bool,
    activity: BTreeMap<CpuCoreKey, CoreActivity>,
    clpc: ClpcSample,
    /// Per-cluster readings of the PMP power histograms (logic + SRAM rails).
    pmp: BTreeMap<ClusterKey, PmpPower>,
}

/// A cluster's power as read from PMP watt-bucket histograms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PmpPower {
    /// Mean power with the lowest bucket counted as 0 W.
    watts: f32,
    /// What the lowest bucket could be hiding: its upper edge times the share
    /// of samples that fell into it. The true mean lies in
    /// `watts - (bucket width) ..= watts + hidden`.
    hidden: f32,
}

/// Parse a PMP "Energy" histogram channel name into its CPU cluster:
/// `PACC`, `PACC0`, `MACC1`, `EACC0`, each optionally followed by " SRAM".
/// GPU channels (`AGX…`) and anything else return `None`.
fn parse_pmp_cluster(name: &str) -> Option<ClusterKey> {
    let base = name.strip_suffix(" SRAM").unwrap_or(name);
    let kind = match base.chars().next()? {
        'P' => CpuKind::Performance,
        'E' | 'M' => CpuKind::Efficiency,
        _ => return None,
    };
    let rest = base[1..].strip_prefix("ACC")?;
    let cluster = if rest.is_empty() {
        0
    } else {
        parse_usize_ascii(rest)?
    };
    Some((kind, cluster))
}

/// Die index of a PMP group name: `PMP` → 0, `PMP1` → 1.
fn parse_pmp_group(group: &str) -> Option<usize> {
    let rest = group.strip_prefix("PMP")?;
    if rest.is_empty() {
        Some(0)
    } else {
        parse_usize_ascii(rest)
    }
}

/// Parse a PMP histogram bucket label such as `"   2W"` or `" 0.250W"`.
fn parse_watt_label(label: &str) -> Option<f32> {
    label.trim().strip_suffix('W')?.trim().parse().ok()
}

/// Mean power of a PMP watt-bucket histogram: Σ label × hits / Σ hits.
///
/// A bucket's label is its upper edge, and the lowest bucket also holds every
/// sample of an idle cluster, so it is counted as 0 W — the convention mactop
/// and macmon use; reading it at its label turns each idle cluster into
/// phantom watts. What that bucket may hide is returned separately. `None`
/// when the histogram saw no samples.
fn histogram_watts(bins: &[(f32, i64)]) -> Option<PmpPower> {
    let total: i64 = bins.iter().map(|(_, hits)| (*hits).max(0)).sum();
    if total <= 0 {
        return None;
    }
    let weighted: f64 = bins
        .iter()
        .skip(1)
        .map(|(label, hits)| *label as f64 * (*hits).max(0) as f64)
        .sum();
    let (lowest_label, lowest_hits) = bins[0];
    Some(PmpPower {
        watts: (weighted / total as f64) as f32,
        hidden: (lowest_label as f64 * lowest_hits.max(0) as f64 / total as f64) as f32,
    })
}

/// Split `total` across `weights`. With no usable weights the split is even.
fn split_by_weights(total: f32, weights: &[f64]) -> Vec<f32> {
    if weights.is_empty() {
        return Vec::new();
    }
    let sum: f64 = weights.iter().filter(|w| w.is_finite() && **w > 0.0).sum();
    if sum <= 1e-12 {
        return vec![total / weights.len() as f32; weights.len()];
    }
    weights
        .iter()
        .map(|w| {
            if w.is_finite() && *w > 0.0 {
                (total as f64 * w / sum) as f32
            } else {
                0.0
            }
        })
        .collect()
}

/// Chooses the power source for each sample and fills in what the Energy
/// Model no longer provides.
///
/// Where the Energy Model is live (macOS ≤ 26) its numbers pass through
/// untouched. It counts as gated when CLPC reports are in use, or when its
/// CPU counters stay silent for a second while cores are active, or move
/// again after half a second of such silence (a catch-up), or imply
/// kilowatts; without CLPC it counts as live again once the counters move
/// for eight samples in a row.
/// Where it is gated (macOS 27+, see [`crate::clpc`]):
///
/// * CPU total, DRAM, ANE and the package total come from CLPC energy counters;
/// * cluster totals come from CLPC's per-cluster counter where its ID is
///   known, otherwise the CPU total is divided using the PMP power histograms
///   (M5 family) or, failing that, DVFS residency;
/// * per-core power is always an estimate: the cluster's power divided by each
///   core's V²·f-weighted active residency. Against the exact per-core energy
///   an entitled sampler publishes, this was within ±5 % for loaded cores on an
///   M5 Max (worst core 14 %);
/// * with neither CLPC nor PMP, the CPU rows stay at zero and
///   `cpu_unavailable` is set.
pub struct SocEstimator {
    /// CLPC cluster array index → cluster.
    clpc_clusters: Vec<ClusterKey>,
    /// Relative switched capacitance of a low-tier core vs a top-tier core,
    /// for dividing CPU power between clusters when nothing measures it.
    low_tier_cap: f64,
    gated: bool,
    zero_streak_s: f32,
    /// Consecutive samples in which the Energy Model CPU counters moved.
    advancing_run: u32,
    core_share: BTreeMap<ClusterKey, f32>,
}

impl SocEstimator {
    pub fn new(topology: &CpuTopology) -> Self {
        let clpc_clusters = topology
            .clusters()
            .into_iter()
            .map(|(tier, idx)| match tier {
                Tier::Low => (CpuKind::Efficiency, idx),
                Tier::High => (CpuKind::Performance, idx),
            })
            .collect();
        // Rough per-core capacitance relative to a top-tier core: an M5
        // "Performance" core measured ≈ 0.55 of a "Super" core at equal V²·f;
        // efficiency cores are several times smaller still.
        let low_tier_cap = match topology.low_tier_type() {
            Some(b'M') => 0.55,
            _ => 0.2,
        };
        Self {
            clpc_clusters,
            low_tier_cap,
            gated: false,
            zero_streak_s: 0.0,
            advancing_run: 0,
            core_share: BTreeMap::new(),
        }
    }

    /// Seed the per-cluster core share with the machine's own long-run ratio
    /// (see [`IOReportSampler::core_share_prior`]).
    pub fn seed_core_shares(&mut self, shares: impl IntoIterator<Item = ((bool, usize), f32)>) {
        for ((high, cluster), share) in shares {
            let kind = if high {
                CpuKind::Performance
            } else {
                CpuKind::Efficiency
            };
            if share.is_finite() {
                self.core_share
                    .entry((kind, cluster))
                    .or_insert(share.clamp(0.3, 1.0));
            }
        }
    }

    /// True once the Energy Model has been found gated.
    pub fn is_gated(&self) -> bool {
        self.gated
    }

    fn share(&self, key: ClusterKey) -> f32 {
        self.core_share
            .get(&key)
            .copied()
            .unwrap_or(DEFAULT_CORE_SHARE)
    }

    /// A publication by an entitled sampler carries exact per-core and
    /// per-cluster energy for the same (unknown) span, so their ratio is exact
    /// whatever the span was.
    fn learn_core_shares(&mut self, raw: &RawSoc) {
        let mut learn = |key: ClusterKey, total: f32, cores: f32| {
            if total > 0.0 && cores > 0.0 && total.is_finite() && cores.is_finite() {
                self.core_share.insert(key, (cores / total).clamp(0.3, 1.0));
            }
        };
        for ((cluster, _), cl) in raw.e_clusters.iter().zip(&raw.em.ecpu_clusters) {
            let cores: f32 = cl.cores.iter().map(|c| c.watts).sum();
            learn((CpuKind::Efficiency, *cluster), cl.total_w, cores);
        }
        let p_total = raw.em.pcpu_cluster.total_w;
        let p_cores: f32 = raw.em.pcpu_cluster.cores.iter().map(|c| c.watts).sum();
        let p_clusters: BTreeSet<usize> = raw.p_cores.iter().map(|k| k.cluster).collect();
        for cluster in p_clusters {
            learn((CpuKind::Performance, cluster), p_total, p_cores);
        }
    }

    pub fn assemble(&mut self, raw: RawSoc) -> SocPower {
        let has_clpc = raw.clpc.cpu_w.is_some();
        // Power the histograms saw above their lowest bucket: proof, by
        // itself, that the CPU is drawing power.
        let pmp_seen: f32 = raw.pmp.values().map(|p| p.watts).sum();
        let cores_active = raw.activity.values().any(|a| a.active > 0.01);

        if has_clpc {
            // CLPC reports are only requested on OS versions that gate the
            // Energy Model; whatever it publishes there is a catch-up.
            self.gated = true;
        } else if raw.em_cpu_advanced {
            self.advancing_run = self.advancing_run.saturating_add(1);
            if self.gated {
                // Counters that move sample after sample are being refreshed
                // at least as often as we look (an entitled sampler such as
                // powermetrics is running, or the gate is gone): live again.
                if self.advancing_run >= UNGATE_AFTER && raw.em.cpu_w <= SPIKE_LIMIT_W {
                    self.gated = false;
                    self.zero_streak_s = 0.0;
                }
            } else if self.zero_streak_s >= CATCH_UP_AFTER_S || raw.em.cpu_w > SPIKE_LIMIT_W {
                // Movement after a silence too long for a live counter is a
                // catch-up for that silence, not a reading of this interval.
                self.gated = true;
            } else {
                self.zero_streak_s = 0.0;
            }
        } else if cores_active {
            self.advancing_run = 0;
            self.zero_streak_s += raw.dt_s;
            // PMP seeing energy that a live mJ counter could not have missed
            // is conclusive on its own; otherwise wait out the streak.
            if self.zero_streak_s >= GATE_AFTER_S || pmp_seen * raw.dt_s > PMP_CONCLUSIVE_J {
                self.gated = true;
            }
        }

        if !self.gated {
            return raw.em;
        }
        if raw.em_cpu_advanced {
            self.learn_core_shares(&raw);
        }

        let RawSoc {
            em: mut soc,
            e_clusters,
            p_cores,
            activity,
            clpc,
            pmp,
            ..
        } = raw;

        // ── clusters, in display order ──────────────────────────────────────
        let mut clusters: Vec<(ClusterKey, Vec<CpuCoreKey>)> = e_clusters
            .iter()
            .map(|(cluster, cores)| ((CpuKind::Efficiency, *cluster), cores.clone()))
            .collect();
        let mut p_groups: BTreeMap<usize, Vec<CpuCoreKey>> = BTreeMap::new();
        for key in &p_cores {
            p_groups.entry(key.cluster).or_default().push(*key);
        }
        clusters.extend(
            p_groups
                .into_iter()
                .map(|(cluster, cores)| ((CpuKind::Performance, cluster), cores)),
        );
        let dyn_of = |key: &CpuCoreKey| activity.get(key).map(|a| a.dyn_weight).unwrap_or(0.0);

        // ── PMP histograms ──────────────────────────────────────────────────
        // A cluster that never leaves its lowest bucket reads 0 W whether it
        // idles or draws a watt. Its cores' active time says which: count the
        // hidden part in proportion to it.
        let pmp_w: Vec<f32> = clusters
            .iter()
            .map(|(key, cores)| {
                let Some(p) = pmp.get(key) else {
                    return 0.0;
                };
                let active = if cores.is_empty() {
                    0.0
                } else {
                    cores
                        .iter()
                        .map(|c| activity.get(c).map_or(0.0, |a| a.active))
                        .sum::<f64>()
                        / cores.len() as f64
                };
                p.watts + p.hidden * active.clamp(0.0, 1.0) as f32
            })
            .collect();
        let pmp_total: Option<f32> = (!pmp.is_empty()).then(|| pmp_w.iter().sum());

        // ── CPU total ───────────────────────────────────────────────────────
        let cpu_total = clpc.cpu_w.or(pmp_total);

        // ── cluster totals ──────────────────────────────────────────────────
        let exact: Option<Vec<f32>> = clpc.clusters_w.as_ref().and_then(|arr| {
            let cpu = clpc.cpu_w?;
            let sum: f32 = arr.iter().sum();
            let consistent = (sum - cpu).abs() <= 0.05 * cpu.abs() + 0.05;
            let covered = arr.len() == self.clpc_clusters.len()
                && clusters
                    .iter()
                    .all(|(key, _)| self.clpc_clusters.contains(key));
            (consistent && covered).then(|| {
                clusters
                    .iter()
                    .map(|(key, _)| {
                        self.clpc_clusters
                            .iter()
                            .position(|k| k == key)
                            .map(|i| arr[i])
                            .unwrap_or(0.0)
                    })
                    .collect()
            })
        });
        let clusters_exact = exact.is_some();
        let cluster_w: Vec<f32> = match (exact, cpu_total) {
            (Some(w), _) => w,
            (None, Some(total)) => {
                let measured: Vec<f64> = pmp_w.iter().map(|w| *w as f64).collect();
                let weights = if measured.iter().sum::<f64>() > 0.0 {
                    measured
                } else {
                    // Nothing measures the clusters (no PMP, or every cluster
                    // in its lowest bucket): fall back to switching activity.
                    clusters
                        .iter()
                        .map(|((kind, _), cores)| {
                            let cap = match kind {
                                CpuKind::Efficiency => self.low_tier_cap,
                                CpuKind::Performance => 1.0,
                            };
                            cap * cores.iter().map(dyn_of).sum::<f64>()
                        })
                        .collect()
                };
                split_by_weights(total, &weights)
            }
            (None, None) => vec![0.0; clusters.len()],
        };

        // ── cores ───────────────────────────────────────────────────────────
        let mut core_w: BTreeMap<CpuCoreKey, f32> = BTreeMap::new();
        for ((key, cores), watts) in clusters.iter().zip(&cluster_w) {
            let weights: Vec<f64> = cores.iter().map(dyn_of).collect();
            let pool = watts * self.share(*key);
            for (core, w) in cores.iter().zip(split_by_weights(pool, &weights)) {
                core_w.insert(*core, w);
            }
        }

        let watts_of = |key: &ClusterKey| -> f32 {
            clusters
                .iter()
                .position(|(k, _)| k == key)
                .map(|i| cluster_w[i])
                .unwrap_or(0.0)
        };
        for ((cluster, cores), cl) in e_clusters.iter().zip(soc.ecpu_clusters.iter_mut()) {
            cl.total_w = watts_of(&(CpuKind::Efficiency, *cluster));
            for (key, core) in cores.iter().zip(cl.cores.iter_mut()) {
                core.watts = core_w.get(key).copied().unwrap_or(0.0);
            }
        }
        soc.pcpu_cluster.total_w = clusters
            .iter()
            .zip(&cluster_w)
            .filter(|(((kind, _), _), _)| *kind == CpuKind::Performance)
            .map(|(_, w)| *w)
            .sum();
        for (key, core) in p_cores.iter().zip(soc.pcpu_cluster.cores.iter_mut()) {
            core.watts = core_w.get(key).copied().unwrap_or(0.0);
        }
        soc.cpu_w = cpu_total.unwrap_or(0.0);

        // ── the other PMGR rails ────────────────────────────────────────────
        // GPU keeps its own live counter (`GPU Energy`, AGX driver). PCIe
        // energy isn't PMGR's either: the PCIe drivers publish it, and their
        // reports stay live under the gate (link-state residency keeps
        // counting), so it is kept as read and only taken out of the package
        // remainder. (An M5 Max reads 0 W there all the same: its device tree
        // gives every PCIe link state a power of 0.) Everything PMGR itself
        // lists is frozen: take what CLPC has and file the remainder of the
        // package under fabric so the SoC total is right.
        soc.ane_parts.clear();
        soc.gpu_sram_w = 0.0;
        soc.isp_w = 0.0;
        soc.display_soc_w = 0.0;
        soc.display_ext_w = 0.0;
        soc.media_w = 0.0;
        if has_clpc {
            soc.ane_w = clpc.ane_w.unwrap_or(0.0);
            soc.dram_w = clpc.dram_w.unwrap_or(0.0);
            let dcs = clpc.dcs_w.unwrap_or(0.0);
            let rest = clpc.package_w.map_or(0.0, |package| {
                let known = soc.cpu_w
                    + clpc.gpu_w.unwrap_or(soc.gpu_w)
                    + soc.ane_w
                    + soc.dram_w
                    + dcs
                    + soc.pcie_w;
                (package - known).max(0.0)
            });
            soc.fabric_w = dcs + rest;
            // Without the package counter fabric is DCS alone and the rails
            // zeroed above stay unknown; ANE and DRAM are live either way.
            soc.rails_merged = clpc.package_w.is_some();
        } else {
            soc.ane_w = 0.0;
            soc.dram_w = 0.0;
            soc.fabric_w = 0.0;
            soc.rails_unavailable = true;
        }

        soc.energy_model_gated = true;
        soc.cores_estimated = true;
        soc.clusters_estimated = !clusters_exact;
        soc.cpu_estimated = !has_clpc && cpu_total.is_some();
        soc.cpu_unavailable = cpu_total.is_none();
        soc.compute_total();
        soc
    }
}

/// Find the running CLPC driver(s) and the energy reports known for them.
/// Returns `(IORegistry entry ID, report)` pairs; empty when the driver or
/// the OS version isn't in the table.
unsafe fn find_clpc_reports() -> Vec<(u64, ClpcReport)> {
    use crate::iokit_ffi::*;

    let mut out = Vec::new();
    let Some(os_major) = crate::topology::os_major_version() else {
        return out;
    };
    let matching = IOServiceMatching(b"AppleCLPC\0".as_ptr() as *const i8);
    if matching.is_null() {
        return out;
    }
    let mut iter: u32 = 0;
    if IOServiceGetMatchingServices(0, matching, &mut iter) != 0 {
        return out;
    }
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }
        let mut driver: u64 = 0;
        let have_id = IORegistryEntryGetRegistryEntryID(entry, &mut driver) == 0;
        let mut props = ptr::null_mut();
        let mut bundle = None;
        if IORegistryEntryCreateCFProperties(entry, &mut props, ptr::null(), 0) == 0
            && !props.is_null()
        {
            let val = cf_utils::cfdict_get(props as CFDictionaryRef, "CFBundleIdentifier");
            if !val.is_null()
                && CFGetTypeID(val) == core_foundation_sys::string::CFStringGetTypeID()
            {
                bundle = cf_utils::cfstring_to_string(val as CFStringRef);
            }
            cf_utils::cf_release(props as _);
        }
        IOObjectRelease(entry);
        if let (true, Some(bundle)) = (have_id, bundle) {
            if let Some(reports) = clpc::energy_reports(&bundle, os_major) {
                out.extend(reports.into_iter().map(|r| (driver, r)));
            }
        }
    }
    IOObjectRelease(iter);
    out
}

/// Describe a CLPC report the way IOReport describes a legend channel, so it
/// can be subscribed to although the driver doesn't advertise it.
fn clpc_channel(driver: u64, report: &ClpcReport) -> CFMutableDictionary<CFString, CFType> {
    // IOReportUnit for "nJ": energy quantity, nano scale.
    const UNIT_NJ: i64 = (3i64 << 56) | (118i64 << 32);
    let legend = CFArray::from_CFTypes(&[
        CFNumber::from(report.id as i64).as_CFType(),
        CFNumber::from(report.channel_type()).as_CFType(),
        CFString::new(report.name()).as_CFType(),
    ]);
    let mut info = CFMutableDictionary::<CFString, CFType>::new();
    info.set(
        CFString::new("IOReportChannelUnit"),
        CFNumber::from(UNIT_NJ).as_CFType(),
    );
    let mut ch = CFMutableDictionary::<CFString, CFType>::new();
    ch.set(
        CFString::new("DriverID"),
        CFNumber::from(driver as i64).as_CFType(),
    );
    ch.set(
        CFString::new("DriverName"),
        CFString::new("AppleCLPC").as_CFType(),
    );
    ch.set(
        CFString::new("IOReportGroupName"),
        CFString::new(clpc::GROUP).as_CFType(),
    );
    ch.set(
        CFString::new("IOReportSubGroupName"),
        CFString::new(clpc::SUBGROUP).as_CFType(),
    );
    ch.set(CFString::new("IOReportChannelInfo"), info.as_CFType());
    ch.set(CFString::new("LegendChannel"), legend.as_CFType());
    ch
}

/// The values of a channel's raw IOReport elements (`IOReportElement`:
/// 32 bytes of header, then four 64-bit values). Unused slots of an array
/// report hold `i64::MIN` and are dropped.
unsafe fn raw_element_values(ch: CFDictionaryRef) -> Vec<i64> {
    const ELEMENT: usize = 64;
    const VALUES_AT: usize = 32;
    let raw = cf_utils::cfdict_get(ch, "RawElements");
    if raw.is_null() || CFGetTypeID(raw) != CFDataGetTypeID() {
        return Vec::new();
    }
    let data = raw as CFDataRef;
    let len = CFDataGetLength(data);
    if len <= 0 {
        return Vec::new();
    }
    let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data), len as usize);
    let mut values = Vec::new();
    let mut element = 0;
    while element + ELEMENT <= bytes.len() {
        let mut at = element + VALUES_AT;
        while at + 8 <= element + ELEMENT {
            let mut v = [0u8; 8];
            v.copy_from_slice(&bytes[at..at + 8]);
            let v = i64::from_le_bytes(v);
            if v != i64::MIN {
                values.push(v);
            }
            at += 8;
        }
        element += ELEMENT;
    }
    values
}

/// Absolute values of the CLPC array report `id` in a sample. `hint` is the
/// channel's position in the delta, which normally matches the sample's.
unsafe fn clpc_array_values(sample: CFDictionaryRef, id: u64, hint: isize) -> Option<Vec<i64>> {
    let arr = cf_utils::cfdict_get(sample, "IOReportChannels") as CFArrayRef;
    if arr.is_null() {
        return None;
    }
    let n = cf_utils::cfarray_len(arr);
    let matches = |i: isize| -> Option<CFDictionaryRef> {
        let ch = cf_utils::cfarray_get(arr, i) as CFDictionaryRef;
        (IOReportChannelGetChannelID(ch) == id
            && cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch)).as_deref()
                == Some(clpc::GROUP))
        .then_some(ch)
    };
    let ch = (hint >= 0 && hint < n)
        .then(|| matches(hint))
        .flatten()
        .or_else(|| (0..n).find_map(matches))?;
    let values = raw_element_values(ch);
    (!values.is_empty()).then_some(values)
}

/// IOReport groups holding the PMP power histograms: the bare group on
/// single-die chips, one numbered group per die otherwise.
const PMP_GROUPS: &[&str] = &["PMP", "PMP0", "PMP1", "PMP2", "PMP3"];

/// Subscribe to the legend channels macpow reads, plus the given CLPC reports
/// (which aren't in any legend, so their descriptions are appended by hand)
/// and, optionally, the PMP power histograms.
unsafe fn subscribe(
    clpc_reports: &[(u64, ClpcReport)],
    with_pmp: bool,
) -> Option<(IOReportSubscriptionRef, CFMutableDictionaryRef)> {
    let mut channels: Vec<(&str, Option<&str>)> = vec![
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
    if with_pmp {
        channels.extend(PMP_GROUPS.iter().map(|g| (*g, Some("Energy"))));
    }

    let mut merged: CFDictionaryRef = ptr::null();
    for &(group, subgroup) in &channels {
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
        return None;
    }

    let desired = cf_utils::cfdict_mutable_copy(merged);
    cf_utils::cf_release(merged as _);
    if desired.is_null() {
        return None;
    }
    if !clpc_reports.is_empty() {
        let existing = cf_utils::cfdict_get(desired, "IOReportChannels") as CFArrayRef;
        let all = if existing.is_null() {
            CFArrayCreateMutable(ptr::null(), 0, &kCFTypeArrayCallBacks)
        } else {
            CFArrayCreateMutableCopy(ptr::null(), 0, existing)
        };
        for (driver, report) in clpc_reports {
            let ch = clpc_channel(*driver, report);
            CFArrayAppendValue(all, ch.as_concrete_TypeRef() as *const libc::c_void);
        }
        let key = CFString::new("IOReportChannels");
        CFDictionarySetValue(
            desired,
            key.as_concrete_TypeRef() as *const libc::c_void,
            all as *const libc::c_void,
        );
        cf_utils::cf_release(all as _);
    }

    let mut subd: CFMutableDictionaryRef = ptr::null_mut();
    let sub = IOReportCreateSubscription(ptr::null(), desired, &mut subd, 0, ptr::null());
    cf_utils::cf_release(desired as _);
    if sub.is_null() || subd.is_null() {
        cf_utils::cf_release(sub as _);
        cf_utils::cf_release(subd as _);
        return None;
    }
    Some((sub, subd))
}

/// Which of the `wanted` CLPC reports a subscription really delivers: present
/// in a sample with as many values as described. Empty if none are, or if
/// the samples can't be diffed — a report of another format than described
/// makes `IOReportCreateSamplesDelta` fail for the whole subscription.
unsafe fn probe_clpc(
    sub: IOReportSubscriptionRef,
    subd: CFMutableDictionaryRef,
    wanted: &[(u64, ClpcReport)],
) -> Vec<ClpcReport> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let first = IOReportCreateSamples(sub, subd, ptr::null());
    let second = IOReportCreateSamples(sub, subd, ptr::null());
    let mut delivered = Vec::new();
    if !first.is_null() && !second.is_null() {
        let delta = IOReportCreateSamplesDelta(first, second, ptr::null());
        if !delta.is_null() {
            let arr = cf_utils::cfdict_get(second, "IOReportChannels") as CFArrayRef;
            for i in 0..cf_utils::cfarray_len(arr) {
                let ch = cf_utils::cfarray_get(arr, i) as CFDictionaryRef;
                if cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch)).as_deref()
                    != Some(clpc::GROUP)
                {
                    continue;
                }
                let id = IOReportChannelGetChannelID(ch);
                let Some((_, report)) = wanted.iter().find(|(_, r)| r.id == id) else {
                    continue;
                };
                // An array report must carry exactly the described number of
                // values; a scalar's element has one value and three zeros.
                let values = raw_element_values(ch).len();
                let fits = if report.format == clpc::FORMAT_SIMPLE_ARRAY {
                    values == report.values as usize
                } else {
                    values >= 1
                };
                if fits && !delivered.contains(report) {
                    delivered.push(*report);
                }
            }
            cf_utils::cf_release(delta as _);
        }
    }
    cf_utils::cf_release(first as _);
    cf_utils::cf_release(second as _);
    delivered
}

/// Flag `soc` as having no power source: [`IOReportSampler::new`] failed (as
/// it does in every virtual machine, or under a sandbox that denies the
/// IOReport user client), so no sample will ever fill it in. Without the
/// flags its zeros can't be told from an idle chip.
///
/// Only the flags are set: other sources own some of the other fields.
pub fn mark_unavailable(soc: &mut SocPower) {
    soc.cpu_unavailable = true;
    soc.rails_unavailable = true;
}

pub struct IOReportSampler {
    subscription: IOReportSubscriptionRef,
    subscribed_channels: CFMutableDictionaryRef,
    pub ecpu_freqs: Vec<u32>, // MHz, indexed by DVFS state
    pub pcpu_freqs: Vec<u32>,
    pub gpu_freqs: Vec<u32>,
    dvfs: DvfsTables,
    topology: CpuTopology,
    /// CLPC reports that made it into the subscription.
    clpc_reports: Vec<ClpcReport>,
}

unsafe impl Send for IOReportSampler {}

impl IOReportSampler {
    /// Create a new sampler subscribing to Energy Model + CPU/GPU Stats, plus
    /// the CLPC energy reports and PMP power histograms where they are needed
    /// to replace a gated Energy Model.
    pub fn new() -> Result<Self> {
        unsafe {
            let clpc_wanted = find_clpc_reports();
            let is_clusters = |r: &ClpcReport| r.kind == ClpcKind::Clusters;
            // The PMP histograms are the next best per-cluster source, and the
            // only CPU source at all where CLPC's report IDs aren't known. They
            // only matter where the Energy Model can be gated (macOS 27+).
            let pmp_useful = !matches!(crate::topology::os_major_version(), Some(v) if v < 27);

            let wants_clusters = clpc_wanted.iter().any(|(_, r)| is_clusters(r));
            let with_pmp = pmp_useful && !wants_clusters;
            let Some((mut sub, mut subd)) = subscribe(&clpc_wanted, with_pmp) else {
                bail!("IOReport: subscription failed");
            };

            // IOReport accepts a description of a report the driver doesn't
            // serve and merely leaves it out of the samples; a report of
            // another format than described breaks the delta. So what CLPC
            // really delivers is only known from a probe sample.
            let mut clpc_reports = probe_clpc(sub, subd, &clpc_wanted);
            let usable: Vec<(u64, ClpcReport)> = clpc_wanted
                .iter()
                .filter(|(_, r)| clpc_reports.iter().any(|d| d.id == r.id))
                .copied()
                .collect();
            let need_pmp = pmp_useful && !clpc_reports.iter().any(is_clusters);
            if usable.len() != clpc_wanted.len() || need_pmp != with_pmp {
                // Subscribe again to exactly what works, adding the PMP
                // histograms if the per-cluster report turned out to be absent.
                if let Some((sub2, subd2)) = subscribe(&usable, need_pmp) {
                    cf_utils::cf_release(sub as _);
                    cf_utils::cf_release(subd as _);
                    sub = sub2;
                    subd = subd2;
                    clpc_reports = probe_clpc(sub, subd, &usable);
                }
            }

            // Read DVFS frequency / voltage tables from pmgr
            let dvfs = read_dvfs_tables();

            Ok(Self {
                subscription: sub,
                subscribed_channels: subd,
                ecpu_freqs: dvfs.ecpu.iter().map(|s| s.mhz).collect(),
                pcpu_freqs: dvfs.pcpu.iter().map(|s| s.mhz).collect(),
                gpu_freqs: dvfs.gpu.clone(),
                dvfs,
                topology: CpuTopology::read(),
                clpc_reports,
            })
        }
    }

    /// A power-source selector configured for this machine. Keep one for the
    /// lifetime of the sampling loop: it remembers whether the Energy Model
    /// turned out to be gated.
    pub fn estimator(&self) -> SocEstimator {
        SocEstimator::new(&self.topology)
    }

    /// CPU topology as read from the device tree (empty if unavailable).
    pub fn topology(&self) -> &CpuTopology {
        &self.topology
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
                    if group == clpc::GROUP {
                        // Synthesized channels: print the report's raw values
                        // (one for a scalar, one per cluster for the array).
                        let count = self
                            .clpc_reports
                            .iter()
                            .find(|r| r.id == IOReportChannelGetChannelID(ch))
                            .map_or(1, |r| r.values as usize);
                        let vals: Vec<String> = raw_element_values(ch)
                            .iter()
                            .take(count)
                            .map(|v| v.to_string())
                            .collect();
                        println!(
                            "{:<20} {:<40} {:<30} {:<8} {}",
                            group,
                            subgroup,
                            name,
                            unit,
                            vals.join(" ")
                        );
                        continue;
                    }
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
                let mv = |t: &[DvfsState]| t.iter().map(|s| s.mv).collect::<Vec<_>>();
                println!("E-CPU core mV:     {:?}", mv(&self.dvfs.ecpu));
                println!("P-CPU core mV:     {:?}", mv(&self.dvfs.pcpu));
                println!("\n--- CPU topology (device tree) ---");
                println!("clusters: {:?}", self.topology.clusters());
                println!(
                    "tier names [low, high]: {:?}",
                    crate::topology::tier_names()
                );
                println!(
                    "CLPC reports delivered: {:?}",
                    self.clpc_reports
                        .iter()
                        .map(|r| r.name())
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// Long-run share of each cluster's energy that the Energy Model books to
    /// the cores themselves, from the cumulative counters in `sample`. The
    /// counters may be stale (gated), but their ratio since boot is still the
    /// machine's own. Keyed by `(top tier?, cluster number)`.
    pub fn core_share_prior(&self, sample: &Sample) -> Vec<((bool, usize), f32)> {
        let mut cores: BTreeMap<ClusterKey, i64> = BTreeMap::new();
        let mut totals: BTreeMap<ClusterKey, i64> = BTreeMap::new();
        unsafe {
            let arr = cf_utils::cfdict_get(sample.inner, "IOReportChannels") as CFArrayRef;
            if arr.is_null() {
                return Vec::new();
            }
            for i in 0..cf_utils::cfarray_len(arr) {
                let ch = cf_utils::cfarray_get(arr, i) as CFDictionaryRef;
                if cf_utils::cfstring_to_string(IOReportChannelGetGroup(ch)).as_deref()
                    != Some("Energy Model")
                {
                    continue;
                }
                let name = cf_utils::cfstring_to_string(IOReportChannelGetChannelName(ch))
                    .unwrap_or_default();
                let (die, base) = strip_die_prefix(&name);
                let offset = die * CLUSTERS_PER_DIE;
                let val = IOReportSimpleGetIntegerValue(ch, 0);
                if val <= 0 {
                    continue;
                }
                if let Some(key) = parse_energy_core_key(base) {
                    *cores.entry((key.kind, key.cluster + offset)).or_default() += val;
                } else if let Some((kind, cluster)) = parse_energy_cluster_total(base) {
                    *totals.entry((kind, cluster + offset)).or_default() += val;
                }
            }
        }
        totals
            .into_iter()
            .filter_map(|(key, total)| {
                let c = *cores.get(&key)?;
                (total > 0 && c > 0 && c <= total).then(|| {
                    (
                        (key.0 == CpuKind::Performance, key.1),
                        c as f32 / total as f32,
                    )
                })
            })
            .collect()
    }

    /// Compute the delta between two samples and turn it into SoC power,
    /// choosing the source per [`SocEstimator`]. This convenience form keeps no
    /// state between calls; a sampling loop should hold an [`estimator`] and
    /// feed it [`parse_raw`] results instead.
    ///
    /// [`estimator`]: Self::estimator
    /// [`parse_raw`]: Self::parse_raw
    pub fn parse_power(&self, prev: &Sample, cur: &Sample) -> Result<SocPower> {
        let mut estimator = self.estimator();
        estimator.seed_core_shares(self.core_share_prior(cur));
        Ok(estimator.assemble(self.parse_raw(prev, cur)?))
    }

    /// Compute the delta between two samples and parse every power source in it.
    pub fn parse_raw(&self, prev: &Sample, cur: &Sample) -> Result<RawSoc> {
        let dt_ms = cur.ts.duration_since(prev.ts).as_millis() as u64;
        let dt_s = dt_ms.max(1) as f32 / 1000.0;

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
            let mut raw = RawSoc {
                dt_s,
                ..Default::default()
            };

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
                    let mut idle_res: i64 = 0;
                    for s in 0..offset {
                        let r = IOReportStateGetResidency(ch, s);
                        if r > 0 {
                            idle_res += r;
                        }
                    }
                    if is_gpu {
                        gpu_idle_res += idle_res;
                    }

                    // Per-core activity: how long the core ran, weighted by the
                    // V²·f of the DVFS state it ran in.
                    let table: &[DvfsState] = if is_pcpu {
                        &self.dvfs.pcpu
                    } else {
                        &self.dvfs.ecpu
                    };
                    let mut active_res: i64 = 0;
                    let mut dyn_res: f64 = 0.0;

                    for s in offset..state_count {
                        let residency = IOReportStateGetResidency(ch, s);
                        if residency <= 0 {
                            continue;
                        }
                        if is_gpu {
                            gpu_active_res += residency;
                        }
                        let idx = (s - offset) as usize;
                        if !is_gpu {
                            active_res += residency;
                            let weight = table
                                .get(idx)
                                .or(table.last())
                                .map(|st| st.dyn_weight())
                                .unwrap_or(1.0);
                            dyn_res += residency as f64 * weight;
                        }
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

                    if let Some(key) = stats_key {
                        let total = (idle_res + active_res) as f64;
                        if total > 0.0 {
                            raw.activity.insert(
                                key,
                                CoreActivity {
                                    dyn_weight: dyn_res / total,
                                    active: active_res as f64 / total,
                                },
                            );
                        }
                    }
                    continue;
                }

                if group == clpc::GROUP {
                    let Some(kind) = clpc::kind_from_name(&name) else {
                        continue;
                    };
                    let to_watts = |nj: i64| nj as f32 / 1e9 / dt_s;
                    if kind == ClpcKind::Clusters {
                        let id = IOReportChannelGetChannelID(ch);
                        let before = clpc_array_values(prev.inner, id, i);
                        let after = clpc_array_values(cur.inner, id, i);
                        let expected = self
                            .clpc_reports
                            .iter()
                            .find(|r| r.kind == ClpcKind::Clusters)
                            .map(|r| r.values as usize);
                        if let (Some(a), Some(b)) = (before, after) {
                            if a.len() == b.len()
                                && Some(a.len()) == expected
                                && a.iter().zip(&b).all(|(x, y)| y >= x)
                            {
                                raw.clpc.clusters_w =
                                    Some(a.iter().zip(&b).map(|(x, y)| to_watts(y - x)).collect());
                            }
                        }
                        continue;
                    }
                    let val = IOReportSimpleGetIntegerValue(ch, 0);
                    if val < 0 {
                        continue;
                    }
                    let slot = match kind {
                        ClpcKind::Cpu => &mut raw.clpc.cpu_w,
                        ClpcKind::Gpu => &mut raw.clpc.gpu_w,
                        ClpcKind::Ane => &mut raw.clpc.ane_w,
                        ClpcKind::Dram => &mut raw.clpc.dram_w,
                        ClpcKind::Dcs => &mut raw.clpc.dcs_w,
                        ClpcKind::Package => &mut raw.clpc.package_w,
                        ClpcKind::Clusters => continue,
                    };
                    *slot = Some(slot.unwrap_or(0.0) + to_watts(val));
                    continue;
                }

                if let Some(die) = parse_pmp_group(&group) {
                    let Some((kind, cluster)) = parse_pmp_cluster(&name) else {
                        continue;
                    };
                    let bins: Vec<(f32, i64)> = (0..IOReportStateGetCount(ch))
                        .filter_map(|s| {
                            let label =
                                cf_utils::cfstring_to_string(IOReportStateGetNameForIndex(ch, s))?;
                            Some((parse_watt_label(&label)?, IOReportStateGetResidency(ch, s)))
                        })
                        .collect();
                    if let Some(reading) = histogram_watts(&bins) {
                        let entry = raw
                            .pmp
                            .entry((kind, cluster + die * CLUSTERS_PER_DIE))
                            .or_default();
                        entry.watts += reading.watts;
                        entry.hidden += reading.hidden;
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
                        raw.em_cpu_advanced |= val != 0;
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
                            raw.em_cpu_advanced |= val != 0;
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
                            raw.em_cpu_advanced |= val != 0;
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
                    .iter()
                    .map(|key| {
                        let (name, watts) = ecpu_power.get(key).cloned().unwrap_or_else(|| {
                            (default_core_name(*key, ecpu_cluster_count > 1), 0.0)
                        });
                        crate::types::CpuCore { name, watts }
                    })
                    .collect();
                soc.ecpu_clusters.push(crate::types::CpuCluster {
                    name,
                    total_w,
                    cores,
                });
                raw.e_clusters.push((cluster, keys));
            }

            let p_cores: Vec<_> = ordered_pcpu_keys
                .iter()
                .map(|key| {
                    let (name, watts) = pcpu_power
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| (default_core_name(*key, pcpu_cluster_count > 1), 0.0));
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
            raw.p_cores = ordered_pcpu_keys;

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
            raw.em = soc;
            Ok(raw)
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

    // ── PMP histograms ───────────────────────────────────────────────────────

    #[test]
    fn parses_pmp_cluster_channel_names() {
        assert_eq!(parse_pmp_cluster("PACC"), Some((CpuKind::Performance, 0)));
        assert_eq!(
            parse_pmp_cluster("PACC SRAM"),
            Some((CpuKind::Performance, 0))
        );
        assert_eq!(parse_pmp_cluster("MACC1"), Some((CpuKind::Efficiency, 1)));
        assert_eq!(
            parse_pmp_cluster("MACC0 SRAM"),
            Some((CpuKind::Efficiency, 0))
        );
        assert_eq!(parse_pmp_cluster("EACC0"), Some((CpuKind::Efficiency, 0)));
        assert_eq!(parse_pmp_cluster("PACC1"), Some((CpuKind::Performance, 1)));
        for other in ["AGX", "AGX AFR", "ANE", "PACCx", "ACC", "", "P", "DCS BW"] {
            assert_eq!(parse_pmp_cluster(other), None, "{other}");
        }
    }

    #[test]
    fn parses_pmp_group_names() {
        assert_eq!(parse_pmp_group("PMP"), Some(0));
        assert_eq!(parse_pmp_group("PMP0"), Some(0));
        assert_eq!(parse_pmp_group("PMP1"), Some(1));
        assert_eq!(parse_pmp_group("PMPx"), None);
        assert_eq!(parse_pmp_group("Energy Model"), None);
    }

    #[test]
    fn parses_watt_bucket_labels() {
        assert_eq!(parse_watt_label("   2W"), Some(2.0));
        assert_eq!(parse_watt_label(" 0.250W"), Some(0.25));
        assert_eq!(parse_watt_label("64W"), Some(64.0));
        assert_eq!(parse_watt_label("IDLE"), None);
        assert_eq!(parse_watt_label("W"), None);
    }

    #[test]
    fn histogram_counts_lowest_bucket_as_zero() {
        // An idle cluster: every sample in the lowest bucket, which could be
        // hiding anything up to its upper edge.
        assert_eq!(
            histogram_watts(&[(2.0, 1000), (4.0, 0), (6.0, 0)]),
            Some(PmpPower {
                watts: 0.0,
                hidden: 2.0
            })
        );
        // Half the samples at "4W", half at "6W": nothing hidden.
        let p = histogram_watts(&[(2.0, 0), (4.0, 500), (6.0, 500)]).unwrap();
        assert!((p.watts - 5.0).abs() < 1e-6 && p.hidden == 0.0);
        // Quarter of the time in the lowest bucket.
        let p = histogram_watts(&[(2.0, 250), (4.0, 750)]).unwrap();
        assert!((p.watts - 3.0).abs() < 1e-6 && (p.hidden - 0.5).abs() < 1e-6);
        // No samples at all → no reading, as opposed to a 0 W reading.
        assert_eq!(histogram_watts(&[(2.0, 0), (4.0, 0)]), None);
        assert_eq!(histogram_watts(&[]), None);
    }

    #[test]
    fn splits_by_weights_and_evenly_without_them() {
        let w = split_by_weights(12.0, &[1.0, 2.0, 3.0]);
        assert!(
            (w[0] - 2.0).abs() < 1e-5 && (w[1] - 4.0).abs() < 1e-5 && (w[2] - 6.0).abs() < 1e-5
        );
        assert_eq!(split_by_weights(9.0, &[0.0, 0.0, 0.0]), vec![3.0, 3.0, 3.0]);
        assert_eq!(split_by_weights(9.0, &[]), Vec::<f32>::new());
        let w = split_by_weights(10.0, &[f64::NAN, 1.0, -2.0, 1.0]);
        assert_eq!(w, vec![0.0, 5.0, 0.0, 5.0]);
    }

    // ── DVFS tables ──────────────────────────────────────────────────────────

    /// GPU frequencies of an M5 Max: `sgx` perf-states less the off state.
    const M5_MAX_GPU_MHZ: [u32; 13] = [
        338, 486, 636, 796, 888, 988, 1084, 1182, 1278, 1374, 1470, 1578, 1620,
    ];

    /// pmgr `voltage-states8` of the same machine: not the GPU's table, but
    /// the longest one in the GPU's frequency range.
    const M5_MAX_STATES8_MHZ: [u32; 26] = [
        732, 816, 984, 1068, 1212, 1284, 1428, 1500, 1584, 1656, 1740, 1800, 1872, 1932, 2016,
        2076, 2112, 2172, 2196, 2244, 2256, 2292, 2304, 2352, 2364, 2472,
    ];

    /// The `perf-states` property of the `sgx` node as read on an M5 Max
    /// (Mac17,7): 4 voltage bins × 14 states × (Hz, mV). On that node
    /// `perf-state-count` is 14 and `gpu-num-perf-states` is 13.
    fn m5_max_perf_states() -> Vec<u8> {
        let hex = concat!(
            "000000007d000000807825147602000080c5f71c7b0200000097e825",
            "a302000000ff712fda02000000ceed34ee02000000afe33a07030000",
            "00879c402f03000080e373463903000080bb2c4c660300008093e551",
            "7f030000806b9e57ac030000805e0e5ef7030000003d8f600b040000",
            "000000007d000000807825145302000080c5f71c7b0200000097e825",
            "9402000000ff712fd502000000ceed34df02000000afe33af8020000",
            "00879c401103000080e373462503000080bb2c4c570300008093e551",
            "70030000806b9e57a7030000805e0e5ee8030000003d8f600b040000",
            "000000007d000000807825145302000080c5f71c7b0200000097e825",
            "9402000000ff712fd502000000ceed34df02000000afe33af8020000",
            "00879c401103000080e373462503000080bb2c4c570300008093e551",
            "70030000806b9e57a7030000805e0e5ee8030000003d8f600b040000",
            "000000007d000000807825147602000080c5f71c7b0200000097e825",
            "a302000000ff712fda02000000ceed34ee02000000afe33a07030000",
            "00879c402f03000080e373463903000080bb2c4c660300008093e551",
            "7f030000806b9e57ac030000805e0e5ef7030000003d8f600b040000",
        );
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reads_gpu_frequencies_from_sgx_perf_states() {
        let blob = m5_max_perf_states();
        assert_eq!(blob.len(), 4 * 14 * 8);
        assert_eq!(dt_u32(&[0x0e, 0, 0, 0]), Some(14));
        assert_eq!(dt_u32(&[0x0d, 0, 0]), None);
        assert_eq!(
            gpu_freqs_from_sgx(&blob, 14, Some(13)).as_deref(),
            Some(&M5_MAX_GPU_MHZ[..])
        );
        // A node that doesn't announce its active states, or holds one bin.
        assert_eq!(
            gpu_freqs_from_sgx(&blob, 14, None).as_deref(),
            Some(&M5_MAX_GPU_MHZ[..])
        );
        assert_eq!(
            gpu_freqs_from_sgx(&blob[..14 * 8], 14, Some(13)).as_deref(),
            Some(&M5_MAX_GPU_MHZ[..])
        );
    }

    #[test]
    fn sgx_perf_states_of_another_shape_are_not_trusted() {
        let blob = m5_max_perf_states();
        // Not the number of active states the node announces.
        assert_eq!(gpu_freqs_from_sgx(&blob, 14, Some(12)), None);
        // Not a whole number of `perf-state-count` tables, or of pairs.
        assert_eq!(gpu_freqs_from_sgx(&blob, 15, None), None);
        assert_eq!(gpu_freqs_from_sgx(&blob[..blob.len() - 8], 14, None), None);
        assert_eq!(gpu_freqs_from_sgx(&blob[..blob.len() - 3], 14, None), None);
        assert_eq!(gpu_freqs_from_sgx(&blob, 0, None), None);
        assert_eq!(gpu_freqs_from_sgx(&[], 14, None), None);
        // Nothing but off states.
        assert_eq!(gpu_freqs_from_sgx(&[0; 14 * 8], 14, None), None);
    }

    /// What [`gpu_freqs_from_sgx`] makes of four states given as raw words, on
    /// a node that announces no count of its own.
    fn four_states(words: &[u32]) -> Option<Vec<u32>> {
        let blob: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        gpu_freqs_from_sgx(&blob, 4, None)
    }

    #[test]
    fn sgx_perf_states_that_are_no_gpu_frequencies_are_not_trusted() {
        const M: u32 = 1_000_000;
        // Without `gpu-num-perf-states` the shape proves little, so the
        // numbers have to look like GPU states too. These do:
        let table = [0, 125, 338 * M, 595, 486 * M, 635, 636 * M, 660];
        assert_eq!(four_states(&table), Some(vec![338, 486, 636]));

        // Out of order, or one state twice.
        let unordered = [0, 125, 486 * M, 635, 338 * M, 595, 636 * M, 660];
        assert_eq!(four_states(&unordered), None);
        let repeated = [0, 125, 338 * M, 595, 338 * M, 635, 636 * M, 660];
        assert_eq!(four_states(&repeated), None);
        // A top frequency no GPU has: 3 MHz, 6 GHz (given in kHz).
        assert_eq!(four_states(&[0, 0, 1, 0, 2, 0, 3, 0]), None);
        let too_fast = [0, 125, 338_000, 595, 486_000, 635, 6_000_000, 660];
        assert_eq!(four_states(&too_fast), None);
        // A first word that is no frequency (7 Hz) would be taken for a state
        // and shift every P-state by one.
        let odd_off_state = [7, 125, 338 * M, 595, 486 * M, 635, 636 * M, 660];
        assert_eq!(four_states(&odd_off_state), None);
        // Three-word entries read as pairs. Two such tables are as long as
        // three tables of pairs, so the length fits; the first "table" reads
        // 0, 7 Hz, 595 Hz, 486 MHz.
        let wider: Vec<u32> = table.chunks(2).flat_map(|p| [p[0], p[1], 7]).collect();
        assert_eq!(four_states(&[wider.clone(), wider].concat()), None);
    }

    /// The M5 Max layout: `-sram` tables carry MHz and the SRAM rail voltage,
    /// the same-numbered plain table carries the core rail voltage. The CPU
    /// tables are abridged; the two GPU candidates are complete (in Hz).
    fn m5_like_tables() -> Vec<(String, Vec<(u32, u32)>)> {
        let hz = |mhz: u32| mhz * 1_000_000;
        vec![
            (
                "voltage-states5".to_string(),
                vec![(50103, 620), (40454, 665), (33098, 705), (14222, 1115)],
            ),
            (
                "voltage-states5-sram".to_string(),
                vec![(1308, 760), (1620, 785), (1980, 820), (4608, 1120)],
            ),
            (
                "voltage-states22".to_string(),
                vec![(48761, 610), (39863, 645), (14962, 1020)],
            ),
            (
                "voltage-states22-sram".to_string(),
                vec![(1344, 765), (1644, 795), (4380, 1070)],
            ),
            (
                "voltage-states8".to_string(),
                M5_MAX_STATES8_MHZ
                    .iter()
                    .map(|&f| (hz(f), u32::MAX))
                    .collect(),
            ),
            (
                // The GPU's real table, off state first. (Voltages elided.)
                "voltage-states9".to_string(),
                std::iter::once((0, 125))
                    .chain(M5_MAX_GPU_MHZ.iter().map(|&f| (hz(f), 0)))
                    .collect(),
            ),
        ]
    }

    #[test]
    fn gpu_table_is_the_sgx_one_and_only_guessed_without_it() {
        let raw = m5_like_tables();
        let sgx = gpu_freqs_from_sgx(&m5_max_perf_states(), 14, Some(13));
        assert_eq!(select_dvfs_tables(&raw, sgx).gpu, M5_MAX_GPU_MHZ);
        // No `sgx` node to ask: the guess is what it always was, which on this
        // machine's pmgr tables is the longer `voltage-states8`.
        assert_eq!(select_dvfs_tables(&raw, None).gpu, M5_MAX_STATES8_MHZ);
    }

    #[test]
    fn dvfs_tables_pair_frequencies_with_core_voltages() {
        let t = select_dvfs_tables(&m5_like_tables(), None);
        assert_eq!(
            t.pcpu,
            vec![
                DvfsState { mhz: 1308, mv: 620 },
                DvfsState { mhz: 1620, mv: 665 },
                DvfsState { mhz: 1980, mv: 705 },
                DvfsState {
                    mhz: 4608,
                    mv: 1115
                },
            ]
        );
        assert_eq!(
            t.ecpu,
            vec![
                DvfsState { mhz: 1344, mv: 610 },
                DvfsState { mhz: 1644, mv: 645 },
                DvfsState {
                    mhz: 4380,
                    mv: 1020
                },
            ]
        );
    }

    #[test]
    fn dvfs_voltage_falls_back_to_sram_rail_then_to_unknown() {
        let mut raw = m5_like_tables();
        raw.retain(|(k, _)| k != "voltage-states5");
        let t = select_dvfs_tables(&raw, None);
        assert_eq!(t.pcpu[0], DvfsState { mhz: 1308, mv: 760 });

        // Implausible voltages everywhere → frequency-only weights.
        let raw = vec![(
            "voltage-states5-sram".to_string(),
            vec![(1308, u32::MAX), (1620, u32::MAX), (4608, u32::MAX)],
        )];
        let t = select_dvfs_tables(&raw, None);
        assert_eq!(t.pcpu[2], DvfsState { mhz: 4608, mv: 0 });
        assert!((t.pcpu[2].dyn_weight() - 4.608).abs() < 1e-9);
    }

    #[test]
    fn dyn_weight_is_v_squared_times_f() {
        let s = DvfsState {
            mhz: 4000,
            mv: 1000,
        };
        assert!((s.dyn_weight() - 4.0).abs() < 1e-9);
        let s = DvfsState { mhz: 2000, mv: 500 };
        assert!((s.dyn_weight() - 0.5).abs() < 1e-9);
    }

    // ── no IOReport at all ───────────────────────────────────────────────────

    #[test]
    fn unavailable_ioreport_is_flagged_in_json() {
        let mut soc = SocPower {
            gpu_util_renderer: 7,
            ..Default::default()
        };
        // Flags are serialized only when set: an untouched SocPower looks
        // exactly like an idle chip.
        let json = serde_json::to_value(&soc).unwrap();
        assert!(json.get("cpu_unavailable").is_none());
        assert!(json.get("rails_unavailable").is_none());

        mark_unavailable(&mut soc);
        let json = serde_json::to_value(&soc).unwrap();
        assert_eq!(json["cpu_unavailable"], true);
        assert_eq!(json["rails_unavailable"], true);
        // Nothing else changes, least of all the fields other sources own.
        assert_eq!(soc.gpu_util_renderer, 7);
        assert_eq!(json["total_w"], 0.0);
        assert!(json.get("energy_model_gated").is_none());
    }

    // ── source selection ─────────────────────────────────────────────────────

    fn key(kind: CpuKind, cluster: usize, core: usize) -> CpuCoreKey {
        CpuCoreKey {
            kind,
            cluster,
            core,
        }
    }

    fn m5_max_topology() -> CpuTopology {
        use crate::topology::TopoCpu;
        let mut cpus = Vec::new();
        for (cluster, cluster_type) in [(0u32, b'M'), (1, b'M'), (2, b'P')] {
            for _ in 0..2 {
                cpus.push(TopoCpu {
                    logical_cpu: cpus.len() as u32,
                    cluster,
                    cluster_type,
                });
            }
        }
        CpuTopology { cpus }
    }

    /// Two low-tier clusters and one top-tier cluster of two cores each, with
    /// the Energy Model reading `em_core_w` per core.
    fn raw_soc(em_core_w: f32, activity: [f64; 6]) -> RawSoc {
        use crate::types::{CpuCluster, CpuCore};
        let core = |name: &str| CpuCore {
            name: name.to_string(),
            watts: em_core_w,
        };
        let cluster = |name: &str, cores: Vec<CpuCore>| CpuCluster {
            name: name.to_string(),
            // The cluster channel also covers shared logic and SRAM.
            total_w: cores.iter().map(|c| c.watts).sum::<f32>() * 2.0,
            cores,
        };
        let mut em = SocPower {
            ecpu_clusters: vec![
                cluster("MCPU0", vec![core("MCPU0_0"), core("MCPU0_1")]),
                cluster("MCPU1", vec![core("MCPU1_0"), core("MCPU1_1")]),
            ],
            pcpu_cluster: cluster("PCPU", vec![core("PACC_0"), core("PACC_1")]),
            gpu_w: 1.0,
            ..Default::default()
        };
        em.cpu_w = em.ecpu_total_w() + em.pcpu_total_w();
        em.compute_total();
        let keys = [
            key(CpuKind::Efficiency, 0, 0),
            key(CpuKind::Efficiency, 0, 1),
            key(CpuKind::Efficiency, 1, 0),
            key(CpuKind::Efficiency, 1, 1),
            key(CpuKind::Performance, 0, 0),
            key(CpuKind::Performance, 0, 1),
        ];
        RawSoc {
            dt_s: 0.25,
            em,
            e_clusters: vec![(0, keys[0..2].to_vec()), (1, keys[2..4].to_vec())],
            p_cores: keys[4..6].to_vec(),
            em_cpu_advanced: em_core_w > 0.0,
            activity: keys
                .iter()
                .zip(activity)
                .map(|(k, a)| {
                    (
                        *k,
                        CoreActivity {
                            dyn_weight: a,
                            active: a.min(1.0),
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn core_watts(soc: &SocPower) -> Vec<f32> {
        soc.ecpu_clusters
            .iter()
            .flat_map(|c| c.cores.iter())
            .chain(soc.pcpu_cluster.cores.iter())
            .map(|c| c.watts)
            .collect()
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn live_energy_model_passes_through_untouched() {
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..40 {
            let raw = raw_soc(0.5, [0.2; 6]);
            let expected_cpu = raw.em.cpu_w;
            let soc = est.assemble(raw);
            assert!(!est.is_gated());
            assert!(!soc.energy_model_gated && !soc.cores_estimated && !soc.clusters_estimated);
            assert!(approx(soc.cpu_w, expected_cpu));
            assert_eq!(core_watts(&soc), vec![0.5; 6]);
            assert!(approx(soc.ecpu_clusters[0].total_w, 2.0));
        }
    }

    #[test]
    fn idle_machine_with_quiet_counters_is_not_declared_gated() {
        // No energy and no active cores (deep idle between samples).
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..40 {
            est.assemble(raw_soc(0.0, [0.0; 6]));
        }
        assert!(!est.is_gated());
    }

    #[test]
    fn busy_cores_with_frozen_counters_gate_after_a_second() {
        let mut est = SocEstimator::new(&m5_max_topology());
        // 3 × 0.25 s of zeros: not yet.
        for _ in 0..3 {
            let soc = est.assemble(raw_soc(0.0, [0.5; 6]));
            assert!(!soc.energy_model_gated);
        }
        assert!(!est.is_gated());
        let soc = est.assemble(raw_soc(0.0, [0.5; 6]));
        assert!(est.is_gated());
        // Nothing live to replace it with: say so instead of claiming 0 W.
        assert!(soc.energy_model_gated && soc.cpu_unavailable && soc.rails_unavailable);
        assert!(approx(soc.cpu_w, 0.0));
        // GPU keeps its own live counter.
        assert!(approx(soc.gpu_w, 1.0) && approx(soc.total_w, 1.0));
    }

    #[test]
    fn movement_after_silence_is_a_catch_up_not_a_reading() {
        let mut est = SocEstimator::new(&m5_max_topology());
        // 30 s at ~5 W published into one 250 ms sample: far below the spike
        // limit, but it follows half a second of silence with busy cores.
        est.assemble(raw_soc(0.0, [0.5; 6]));
        est.assemble(raw_soc(0.0, [0.5; 6]));
        assert!(!est.is_gated());
        let soc = est.assemble(raw_soc(50.0, [0.5; 6]));
        assert!(est.is_gated());
        assert!(soc.energy_model_gated && soc.cpu_w < 1.0, "{}", soc.cpu_w);
    }

    #[test]
    fn live_counter_skipping_a_sample_at_idle_is_not_gated() {
        // A few mW at a 100 ms interval: under 1 mJ per sample, so the mJ
        // counter ticks only every second or third sample.
        let mut est = SocEstimator::new(&m5_max_topology());
        for i in 0..300 {
            let mut raw = raw_soc(if i % 3 == 2 { 0.01 } else { 0.0 }, [0.02; 6]);
            raw.dt_s = 0.1;
            let soc = est.assemble(raw);
            assert!(!est.is_gated() && !soc.energy_model_gated, "sample {i}");
        }
    }

    #[test]
    fn periodic_publications_never_look_live() {
        // An entitled sampler publishing once a second, read four times a second.
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..20 {
            for _ in 0..3 {
                est.assemble(raw_soc(0.0, [0.5; 6]));
            }
            let soc = est.assemble(raw_soc(2.0, [0.5; 6]));
            assert!(soc.energy_model_gated && soc.cpu_w < 1.0);
        }
        assert!(est.is_gated());
    }

    #[test]
    fn counters_moving_every_sample_are_trusted_again() {
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..4 {
            est.assemble(raw_soc(0.0, [0.5; 6]));
        }
        assert!(est.is_gated());
        // The gate lifts (or an entitled sampler refreshes at our cadence).
        let mut last = SocPower::default();
        for i in 0..UNGATE_AFTER {
            last = est.assemble(raw_soc(0.5, [0.5; 6]));
            assert_eq!(est.is_gated(), i + 1 < UNGATE_AFTER);
        }
        assert!(!last.energy_model_gated && !last.cores_estimated);
        assert_eq!(core_watts(&last), vec![0.5; 6]);
        // ... and freezing again is noticed again.
        for _ in 0..4 {
            last = est.assemble(raw_soc(0.0, [0.5; 6]));
        }
        assert!(est.is_gated() && last.energy_model_gated);
    }

    #[test]
    fn clpc_keeps_the_estimator_gated_whatever_the_energy_model_does() {
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..3 * UNGATE_AFTER {
            let mut raw = raw_soc(0.5, [0.5; 6]);
            raw.clpc.cpu_w = Some(12.0);
            let soc = est.assemble(raw);
            assert!(soc.energy_model_gated && approx(soc.cpu_w, 12.0));
        }
    }

    #[test]
    fn stray_pmp_event_does_not_gate_a_live_counter() {
        // One sub-millisecond PMP event above the lowest bucket while the mJ
        // counter happens not to tick: far too little energy to prove anything.
        let mut est = SocEstimator::new(&m5_max_topology());
        for _ in 0..20 {
            est.assemble(raw_soc(0.5, [0.02; 6]));
        }
        let mut raw = raw_soc(0.0, [0.02; 6]);
        raw.pmp = pmp([(0.0, 2.0), (0.0, 2.0), (0.0002, 2.0)]);
        let soc = est.assemble(raw);
        assert!(!est.is_gated() && !soc.energy_model_gated);
    }

    #[test]
    fn catch_up_spike_is_discarded_and_gates() {
        let mut est = SocEstimator::new(&m5_max_topology());
        // 8.5 kJ published at once and divided by one 250 ms interval.
        let soc = est.assemble(raw_soc(2800.0, [0.5; 6]));
        assert!(est.is_gated());
        assert!(soc.cpu_w < 1.0, "spike leaked: {} W", soc.cpu_w);
        assert!(soc.total_w < 2.0);
        assert!(core_watts(&soc).iter().all(|w| *w == 0.0));
    }

    #[test]
    fn clpc_cluster_energy_is_used_as_is_and_split_between_cores() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [3.0, 1.0, 0.0, 0.0, 2.0, 2.0]);
        raw.clpc = ClpcSample {
            cpu_w: Some(14.0),
            gpu_w: Some(1.0),
            ane_w: Some(0.5),
            dram_w: Some(2.0),
            dcs_w: Some(1.5),
            package_w: Some(22.0),
            // Kernel cluster order: low 0, low 1, top.
            clusters_w: Some(vec![4.0, 2.0, 8.0]),
        };
        let soc = est.assemble(raw);
        assert!(soc.energy_model_gated && soc.cores_estimated);
        assert!(!soc.clusters_estimated && !soc.cpu_estimated && !soc.cpu_unavailable);
        assert!(approx(soc.cpu_w, 14.0));
        assert!(approx(soc.ecpu_clusters[0].total_w, 4.0));
        assert!(approx(soc.ecpu_clusters[1].total_w, 2.0));
        assert!(approx(soc.pcpu_cluster.total_w, 8.0));
        // Cores share DEFAULT_CORE_SHARE of their cluster, by activity; an
        // all-idle cluster is split evenly.
        let s = DEFAULT_CORE_SHARE;
        let w = core_watts(&soc);
        assert!(approx(w[0], 4.0 * s * 0.75) && approx(w[1], 4.0 * s * 0.25));
        assert!(approx(w[2], 2.0 * s * 0.5) && approx(w[3], 2.0 * s * 0.5));
        assert!(approx(w[4], 8.0 * s * 0.5) && approx(w[5], 8.0 * s * 0.5));
        // Other rails: CLPC values, the unmeasured remainder under fabric.
        assert!(approx(soc.ane_w, 0.5) && approx(soc.dram_w, 2.0));
        assert!(approx(
            soc.fabric_w,
            1.5 + (22.0 - 14.0 - 1.0 - 0.5 - 2.0 - 1.5)
        ));
        assert!(soc.rails_merged && !soc.rails_unavailable);
        // gpu_w stays the live AGX reading; the total adds up to the package.
        assert!(approx(soc.total_w, 22.0));
    }

    #[test]
    fn inconsistent_cluster_energy_is_ignored() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        raw.clpc.cpu_w = Some(10.0);
        // Does not add up to the CPU counter: not the report we think it is.
        raw.clpc.clusters_w = Some(vec![40.0, 2.0, 8.0]);
        let soc = est.assemble(raw);
        assert!(soc.clusters_estimated);
        let sum = soc.ecpu_total_w() + soc.pcpu_total_w();
        assert!(
            approx(sum, 10.0),
            "clusters must still add up to CPU: {sum}"
        );
    }

    /// PMP readings `(watts, hidden)` for low cluster 0, low cluster 1, top cluster.
    fn pmp(readings: [(f32, f32); 3]) -> BTreeMap<ClusterKey, PmpPower> {
        [
            (CpuKind::Efficiency, 0),
            (CpuKind::Efficiency, 1),
            (CpuKind::Performance, 0),
        ]
        .into_iter()
        .zip(readings)
        .map(|(key, (watts, hidden))| (key, PmpPower { watts, hidden }))
        .collect()
    }

    #[test]
    fn busy_cluster_in_its_lowest_pmp_bucket_is_not_read_as_zero() {
        let mut est = SocEstimator::new(&m5_max_topology());
        // Low cluster 0 idles, low cluster 1 is half busy; both sit entirely
        // in their lowest 2 W bucket. The top cluster is well above it.
        let mut raw = raw_soc(0.0, [0.0, 0.0, 0.5, 0.5, 1.0, 1.0]);
        raw.clpc.cpu_w = Some(11.0);
        raw.pmp = pmp([(0.0, 2.0), (0.0, 2.0), (10.0, 0.0)]);
        let soc = est.assemble(raw);
        // Weights 0 : 2×0.5 : 10 of an 11 W total.
        assert!(approx(soc.ecpu_clusters[0].total_w, 0.0));
        assert!(approx(soc.ecpu_clusters[1].total_w, 1.0));
        assert!(approx(soc.pcpu_cluster.total_w, 10.0));
    }

    #[test]
    fn idle_cores_alone_do_not_make_pmp_conclusive() {
        // Everything in the lowest bucket and the counter silent: that is also
        // what a live but idle machine looks like, so only the streak decides.
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [0.5; 6]);
        raw.pmp = pmp([(0.0, 2.0), (0.0, 2.0), (0.0, 2.0)]);
        est.assemble(raw);
        assert!(!est.is_gated());
    }

    #[test]
    fn clpc_without_package_keeps_ane_and_dram_live() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0; 6]);
        raw.clpc = ClpcSample {
            cpu_w: Some(10.0),
            ane_w: Some(0.5),
            dram_w: Some(2.0),
            dcs_w: Some(1.5),
            ..Default::default()
        };
        let soc = est.assemble(raw);
        assert!(!soc.rails_unavailable && !soc.rails_merged);
        assert!(approx(soc.ane_w, 0.5) && approx(soc.dram_w, 2.0) && approx(soc.fabric_w, 1.5));
        assert!(approx(soc.total_w, 10.0 + 1.0 + 0.5 + 2.0 + 1.5));
    }

    #[test]
    fn pcie_reading_is_kept_and_taken_out_of_the_package_remainder() {
        // PCIe energy comes from the PCIe drivers, not from PMGR, so unlike a
        // PMGR rail (ISP here) it stays a rail of its own.
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0; 6]);
        raw.em.pcie_w = 2.0;
        raw.em.isp_w = 0.7;
        raw.clpc = ClpcSample {
            cpu_w: Some(14.0),
            gpu_w: Some(1.0),
            ane_w: Some(0.5),
            dram_w: Some(2.0),
            dcs_w: Some(1.5),
            package_w: Some(22.0),
            clusters_w: None,
        };
        let soc = est.assemble(raw);
        assert!(approx(soc.pcie_w, 2.0) && approx(soc.isp_w, 0.0));
        assert!(approx(
            soc.fabric_w,
            1.5 + (22.0 - 14.0 - 1.0 - 0.5 - 2.0 - 1.5 - 2.0)
        ));
        // Counted once: the total is still the package.
        assert!(soc.rails_merged && approx(soc.total_w, 22.0));
    }

    #[test]
    fn pmp_histograms_divide_the_clpc_total_between_clusters() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0; 6]);
        raw.clpc.cpu_w = Some(12.0);
        // Histograms read 20 % high in absolute terms; only their ratio is used.
        raw.pmp = pmp([(1.2, 0.0), (2.4, 0.0), (10.8, 0.0)]);
        let soc = est.assemble(raw);
        assert!(soc.clusters_estimated && !soc.cpu_estimated);
        assert!(approx(soc.cpu_w, 12.0));
        assert!(approx(soc.ecpu_clusters[0].total_w, 1.0));
        assert!(approx(soc.ecpu_clusters[1].total_w, 2.0));
        assert!(approx(soc.pcpu_cluster.total_w, 9.0));
    }

    #[test]
    fn pmp_alone_gates_immediately_and_supplies_an_estimated_total() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0; 6]);
        raw.pmp = pmp([(2.0, 0.0), (0.0, 0.0), (6.0, 0.0)]);
        let soc = est.assemble(raw);
        assert!(
            est.is_gated(),
            "PMP watts with a silent counter is conclusive"
        );
        assert!(soc.cpu_estimated && soc.clusters_estimated && !soc.cpu_unavailable);
        assert!(approx(soc.cpu_w, 8.0));
        assert!(approx(soc.ecpu_clusters[0].total_w, 2.0));
        assert!(approx(soc.ecpu_clusters[1].total_w, 0.0));
        assert!(approx(soc.pcpu_cluster.total_w, 6.0));
        // No package counter: the remaining rails are unknown, not zero.
        assert!(soc.rails_unavailable && !soc.rails_merged);
    }

    #[test]
    fn without_cluster_measurements_activity_divides_the_total() {
        // CLPC total only (M1–M4 on macOS 27: no PMP histograms, no cluster report).
        let mut est = SocEstimator::new(&m5_max_topology());
        let mut raw = raw_soc(0.0, [1.0, 1.0, 0.0, 0.0, 1.0, 1.0]);
        raw.clpc.cpu_w = Some(15.5);
        let soc = est.assemble(raw);
        // Equal activity, but a top-tier core switches more capacitance (1 : 0.55).
        assert!(approx(soc.pcpu_cluster.total_w, 10.0));
        assert!(approx(soc.ecpu_clusters[0].total_w, 5.5));
        assert!(approx(soc.ecpu_clusters[1].total_w, 0.0));
        assert!(soc.clusters_estimated);
    }

    #[test]
    fn publication_teaches_the_core_share() {
        let mut est = SocEstimator::new(&m5_max_topology());
        let clpc = |raw: &mut RawSoc| {
            raw.clpc.cpu_w = Some(12.0);
            raw.clpc.clusters_w = Some(vec![2.0, 2.0, 8.0]);
        };
        let mut raw = raw_soc(0.0, [1.0; 6]);
        clpc(&mut raw);
        let before = core_watts(&est.assemble(raw));
        assert!(approx(before[4], 8.0 * DEFAULT_CORE_SHARE * 0.5));

        // An entitled sampler publishes: in `raw_soc` cores are half of each
        // cluster's energy, whatever span the catch-up covers.
        let mut raw = raw_soc(900.0, [1.0; 6]);
        clpc(&mut raw);
        let soc = est.assemble(raw);
        assert!(
            approx(soc.cpu_w, 12.0),
            "catch-up must not reach the output"
        );
        let after = core_watts(&soc);
        assert!(approx(after[4], 8.0 * 0.5 * 0.5));
        assert!(approx(after[0], 2.0 * 0.5 * 0.5));
    }

    #[test]
    fn seeded_core_share_is_kept_until_a_publication_replaces_it() {
        let mut est = SocEstimator::new(&m5_max_topology());
        est.seed_core_shares([((true, 0), 0.8), ((false, 1), 0.4), ((false, 0), f32::NAN)]);
        let mut raw = raw_soc(0.0, [1.0; 6]);
        raw.clpc.cpu_w = Some(12.0);
        raw.clpc.clusters_w = Some(vec![2.0, 2.0, 8.0]);
        let w = core_watts(&est.assemble(raw));
        assert!(approx(w[0], 2.0 * DEFAULT_CORE_SHARE * 0.5));
        assert!(approx(w[2], 2.0 * 0.4 * 0.5));
        assert!(approx(w[4], 8.0 * 0.8 * 0.5));
    }
}
