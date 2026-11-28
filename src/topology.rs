//! CPU topology from the Apple Silicon device tree (`IODeviceTree:/cpus`).
//!
//! The kernel numbers logical CPUs in device-tree order, and that order is
//! chip-specific: M1–M4 list the efficiency cluster first, M5 Pro/Max list the
//! two "Performance" (`M`) clusters first and the "Super" (`P`) cluster last.
//! Anything indexed by logical CPU (Mach `host_processor_info`) or by logical
//! cluster (CLPC per-cluster reports) has to go through this table instead of
//! assuming an order.

use crate::cf_utils;
use crate::iokit_ffi::*;
use core_foundation::base::TCFType;
use core_foundation::number::CFNumber;
use core_foundation_sys::base::{CFGetTypeID, CFTypeRef};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef};
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::number::{CFNumberGetTypeID, CFNumberRef};
use core_foundation_sys::string::{CFStringGetTypeID, CFStringRef};

/// One logical CPU as described by the device tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopoCpu {
    /// Kernel logical CPU number (index into `host_processor_info`).
    pub logical_cpu: u32,
    /// Kernel logical cluster number.
    pub cluster: u32,
    /// Device-tree `cluster-type`: `b'E'`, `b'M'` or `b'P'`.
    pub cluster_type: u8,
}

/// Which of macpow's two CPU buckets a cluster belongs to.
///
/// macpow keeps two tiers for display: the top tier (`pcpu_cluster`, every
/// device-tree `P` cluster) and everything below it (`ecpu_clusters`, the `E`
/// and `M` clusters).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Low,
    High,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuTopology {
    /// Sorted by `logical_cpu`.
    pub cpus: Vec<TopoCpu>,
}

impl CpuTopology {
    /// Read the topology from the device tree. Empty when it can't be read;
    /// callers then fall back to their historical assumptions.
    pub fn read() -> Self {
        let mut cpus = unsafe { read_cpus() };
        cpus.sort_by_key(|c| c.logical_cpu);
        cpus.dedup_by_key(|c| c.logical_cpu);
        // Logical CPU ids must be dense (0..n) to be usable as indices.
        let dense = cpus
            .iter()
            .enumerate()
            .all(|(i, c)| c.logical_cpu as usize == i);
        if dense {
            Self { cpus }
        } else {
            Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cpus.is_empty()
    }

    fn tier_of(cluster_type: u8) -> Tier {
        if cluster_type == b'P' {
            Tier::High
        } else {
            Tier::Low
        }
    }

    /// For each logical CPU: does it belong to the top tier (`pcpu_cluster`)?
    pub fn high_tier_mask(&self) -> Vec<bool> {
        self.cpus
            .iter()
            .map(|c| Self::tier_of(c.cluster_type) == Tier::High)
            .collect()
    }

    /// Logical clusters in kernel order, each mapped to `(tier, index within
    /// that tier)`. The index matches the cluster number in IOReport channel
    /// names (`MCPU0`/`MCPU1` → Low 0/1, `PCPU` → High 0).
    pub fn clusters(&self) -> Vec<(Tier, usize)> {
        let mut ids: Vec<(u32, u8)> = Vec::new();
        for c in &self.cpus {
            if !ids.iter().any(|(id, _)| *id == c.cluster) {
                ids.push((c.cluster, c.cluster_type));
            }
        }
        ids.sort_by_key(|(id, _)| *id);
        let (mut low, mut high) = (0usize, 0usize);
        ids.into_iter()
            .map(|(_, ty)| match Self::tier_of(ty) {
                Tier::Low => {
                    low += 1;
                    (Tier::Low, low - 1)
                }
                Tier::High => {
                    high += 1;
                    (Tier::High, high - 1)
                }
            })
            .collect()
    }

    /// Device-tree type letter of the low tier (`b'E'` or `b'M'`), if any.
    /// When both exist (three-tier chips) the smaller core type wins.
    pub fn low_tier_type(&self) -> Option<u8> {
        let mut types = self
            .cpus
            .iter()
            .map(|c| c.cluster_type)
            .filter(|t| *t != b'P');
        let first = types.next()?;
        Some(if first == b'E' || types.any(|t| t == b'E') {
            b'E'
        } else {
            first
        })
    }
}

/// Marketing names of macpow's two CPU tiers as `[low, high]`, taken from
/// `hw.perflevelN.name` (level 0 is the fastest). `["Efficiency",
/// "Performance"]` on M1–M4 and base M5, `["Performance", "Super"]` on M5
/// Pro/Max. With three levels the two slower ones share the low tier.
pub fn tier_names() -> [String; 2] {
    let fallback = || ["Efficiency".to_string(), "Performance".to_string()];
    let levels = sysctl_u32("hw.nperflevels").unwrap_or(0);
    if levels < 2 {
        return fallback();
    }
    let names: Vec<String> = (0..levels)
        .filter_map(|i| sysctl_string(&format!("hw.perflevel{i}.name")))
        .filter(|s| !s.is_empty())
        .collect();
    if names.len() != levels as usize {
        return fallback();
    }
    // Slowest first inside the low tier, e.g. "Efficiency+Performance".
    let low: Vec<&str> = names[1..].iter().rev().map(|s| s.as_str()).collect();
    [low.join("+"), names[0].clone()]
}

/// Major version of the running macOS (`27` for 27.0.1).
pub fn os_major_version() -> Option<u32> {
    sysctl_string("kern.osproductversion")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn sysctl_string(name: &str) -> Option<String> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    unsafe {
        if libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
            || len == 0
            || len > 256
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        buf.truncate(len);
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8(buf[..end].to_vec()).ok()
    }
}

fn sysctl_u32(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut val: u32 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u32>();
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut u32 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u32>()).then_some(val)
}

/// Read a device-tree property that may be a CFNumber or a little-endian CFData.
unsafe fn prop_u32(dict: CFDictionaryRef, key: &str) -> Option<u32> {
    let val: CFTypeRef = cf_utils::cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    let ty = CFGetTypeID(val);
    if ty == CFNumberGetTypeID() {
        let n: CFNumber = TCFType::wrap_under_get_rule(val as CFNumberRef);
        n.to_i64().and_then(|v| u32::try_from(v).ok())
    } else if ty == CFDataGetTypeID() {
        let data = val as CFDataRef;
        let len = CFDataGetLength(data);
        if len < 4 {
            return None;
        }
        let p = CFDataGetBytePtr(data);
        let bytes = std::slice::from_raw_parts(p, 4);
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    } else {
        None
    }
}

/// Read the first byte of a device-tree string property (CFData or CFString).
unsafe fn prop_first_byte(dict: CFDictionaryRef, key: &str) -> Option<u8> {
    let val: CFTypeRef = cf_utils::cfdict_get(dict, key);
    if val.is_null() {
        return None;
    }
    let ty = CFGetTypeID(val);
    if ty == CFDataGetTypeID() {
        let data = val as CFDataRef;
        if CFDataGetLength(data) < 1 {
            return None;
        }
        Some(*CFDataGetBytePtr(data))
    } else if ty == CFStringGetTypeID() {
        cf_utils::cfstring_to_string(val as CFStringRef).and_then(|s| s.bytes().next())
    } else {
        None
    }
}

unsafe fn read_cpus() -> Vec<TopoCpu> {
    let mut out = Vec::new();
    let parent = IORegistryEntryFromPath(0, b"IODeviceTree:/cpus\0".as_ptr() as *const i8);
    if parent == 0 {
        return out;
    }
    let mut iter: u32 = 0;
    if IORegistryEntryGetChildIterator(parent, b"IODeviceTree\0".as_ptr() as *const i8, &mut iter)
        != 0
    {
        IOObjectRelease(parent);
        return out;
    }
    loop {
        let entry = IOIteratorNext(iter);
        if entry == 0 {
            break;
        }
        let mut props = std::ptr::null_mut();
        if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null(), 0) == 0
            && !props.is_null()
        {
            let dict = props as CFDictionaryRef;
            let logical_cpu = prop_u32(dict, "logical-cpu-id").or_else(|| prop_u32(dict, "cpu-id"));
            let cluster =
                prop_u32(dict, "logical-cluster-id").or_else(|| prop_u32(dict, "cluster-id"));
            let cluster_type = prop_first_byte(dict, "cluster-type");
            if let (Some(logical_cpu), Some(cluster), Some(cluster_type)) =
                (logical_cpu, cluster, cluster_type)
            {
                out.push(TopoCpu {
                    logical_cpu,
                    cluster,
                    cluster_type: cluster_type.to_ascii_uppercase(),
                });
            }
            cf_utils::cf_release(props as _);
        }
        IOObjectRelease(entry);
    }
    IOObjectRelease(iter);
    IOObjectRelease(parent);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo(layout: &[(u32, u8, usize)]) -> CpuTopology {
        let mut cpus = Vec::new();
        for &(cluster, cluster_type, count) in layout {
            for _ in 0..count {
                cpus.push(TopoCpu {
                    logical_cpu: cpus.len() as u32,
                    cluster,
                    cluster_type,
                });
            }
        }
        CpuTopology { cpus }
    }

    #[test]
    fn m5_max_lists_performance_clusters_before_super() {
        let t = topo(&[(0, b'M', 6), (1, b'M', 6), (2, b'P', 6)]);
        let mask = t.high_tier_mask();
        assert_eq!(mask.iter().filter(|h| **h).count(), 6);
        assert!(mask[..12].iter().all(|h| !h));
        assert!(mask[12..].iter().all(|h| *h));
        assert_eq!(
            t.clusters(),
            vec![(Tier::Low, 0), (Tier::Low, 1), (Tier::High, 0)]
        );
        assert_eq!(t.low_tier_type(), Some(b'M'));
    }

    #[test]
    fn classic_layout_lists_efficiency_cluster_first() {
        let t = topo(&[(0, b'E', 4), (1, b'P', 4), (2, b'P', 4)]);
        assert_eq!(
            t.clusters(),
            vec![(Tier::Low, 0), (Tier::High, 0), (Tier::High, 1)]
        );
        assert_eq!(
            &t.high_tier_mask()[..5],
            &[false, false, false, false, true]
        );
        assert_eq!(t.low_tier_type(), Some(b'E'));
    }

    #[test]
    fn three_tier_low_bucket_reports_efficiency_type() {
        let t = topo(&[(0, b'M', 4), (1, b'E', 4), (2, b'P', 2)]);
        assert_eq!(t.low_tier_type(), Some(b'E'));
        assert_eq!(
            t.clusters(),
            vec![(Tier::Low, 0), (Tier::Low, 1), (Tier::High, 0)]
        );
    }

    #[test]
    fn empty_topology_yields_empty_views() {
        let t = CpuTopology::default();
        assert!(t.is_empty());
        assert!(t.high_tier_mask().is_empty());
        assert!(t.clusters().is_empty());
        assert_eq!(t.low_tier_type(), None);
    }
}
