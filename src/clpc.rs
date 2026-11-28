//! AppleCLPC energy reports — the live energy source on macOS 27+.
//!
//! macOS 27 made the kernel PMGR driver recompute the IOReport "Energy Model"
//! counters (CPU, per-core, DRAM, ANE, …) only when the *sampling* task holds
//! the private entitlement `com.apple.private.pmgr.nrg.reporting`. Everyone
//! else — root included — reads the values left behind by the last entitled
//! sampler (`powermetrics`, the powerlog daemons): frozen for minutes, then one
//! huge catch-up jump.
//!
//! The CLPC driver publishes the same accumulated energies through reports
//! that are not gated. They are absent from the IOReport legend, so they can't
//! be enumerated; a client has to hand IOReport a channel description carrying
//! the report's opaque ID, `(table index << 32) | key`. Keys differ per SoC
//! driver, indices per macOS major version.
//!
//! The scalar keys (CPU, GPU, ANE, DRAM, DCS, package) come from the macmon
//! project (<https://github.com/vladkens/macmon>, MIT), which extracts them
//! from Apple's kernelcaches. The per-cluster array key was read from the
//! T6050 kernelcache and verified on an M5 Max: its elements sum to the CPU
//! scalar and track the Energy Model cluster channels.

/// IOReport group / subgroup under which the synthesized channels are filed.
pub const GROUP: &str = "CLPC";
pub const SUBGROUP: &str = "Energy Counters";

/// `IOReportChannelType.report_format` values (IOKit/IOReportTypes.h).
pub const FORMAT_SIMPLE: u8 = 1;
pub const FORMAT_SIMPLE_ARRAY: u8 = 4;

/// What a CLPC report measures. Every report is cumulative energy in nJ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClpcKind {
    Cpu,
    Gpu,
    Ane,
    Dram,
    /// DRAM controller / PHY ("DCS" in the Energy Model).
    Dcs,
    /// Whole SoC package.
    Package,
    /// Per-cluster CPU energy, one element per logical cluster.
    Clusters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClpcReport {
    pub kind: ClpcKind,
    pub id: u64,
    pub format: u8,
    /// Number of values the report carries (1 for scalars).
    pub values: u8,
}

impl ClpcReport {
    /// Channel name used for the synthesized IOReport channel.
    pub fn name(&self) -> &'static str {
        match self.kind {
            ClpcKind::Cpu => "CPU Energy",
            ClpcKind::Gpu => "GPU Energy",
            ClpcKind::Ane => "ANE",
            ClpcKind::Dram => "DRAM",
            ClpcKind::Dcs => "DCS",
            ClpcKind::Package => "Package Energy",
            ClpcKind::Clusters => "Cluster Energy",
        }
    }

    /// IOReport elements needed to carry `values` (4 values per element).
    pub fn elements(&self) -> u16 {
        (self.values.max(1) as u16 - 1) / 4 + 1
    }

    /// `IOReportChannelType` packed as the 64-bit integer IOReport expects:
    /// report format, power category, element count.
    pub fn channel_type(&self) -> i64 {
        self.format as i64 | (2 << 16) | ((self.elements() as i64) << 32)
    }
}

pub fn kind_from_name(name: &str) -> Option<ClpcKind> {
    Some(match name {
        "CPU Energy" => ClpcKind::Cpu,
        "GPU Energy" => ClpcKind::Gpu,
        "ANE" => ClpcKind::Ane,
        "DRAM" => ClpcKind::Dram,
        "DCS" => ClpcKind::Dcs,
        "Package Energy" => ClpcKind::Package,
        "Cluster Energy" => ClpcKind::Clusters,
        _ => return None,
    })
}

struct Keys {
    bundle: &'static str,
    cpu: u32,
    gpu: u32,
    ane: u32,
    ram: u32,
    dcs: u32,
    chip: u32,
    /// `(key, cluster count)` of the per-cluster CPU energy array, where known.
    clusters: Option<(u32, u8)>,
}

/// Report-table indices on macOS 27.0 – 27.2 (identical across SoC drivers).
struct Indices {
    cpu: u32,
    clusters: u32,
    gpu: u32,
    ane: u32,
    ram: u32,
    dcs: u32,
    chip: u32,
}

const INDICES_27: Indices = Indices {
    cpu: 16,
    clusters: 17,
    gpu: 25,
    ane: 24,
    ram: 26,
    dcs: 27,
    chip: 9,
};

const fn keys(
    bundle: &'static str,
    cpu: u32,
    gpu: u32,
    ane: u32,
    ram: u32,
    dcs: u32,
    chip: u32,
) -> Keys {
    Keys {
        bundle,
        cpu,
        gpu,
        ane,
        ram,
        dcs,
        chip,
        clusters: None,
    }
}

#[rustfmt::skip]
const KEYS: &[Keys] = &[
    keys("com.apple.driver.AppleT8103CLPCv3", 0x2cdd_31f2, 0x21be_cf42, 0x0667_fb81, 0xc0c2_0839, 0x31d3_e1e7, 0xd685_8d06),
    keys("com.apple.driver.AppleT6000CLPCv3", 0x4747_9059, 0x4519_bf1e, 0x9aaa_17f8, 0xd6ca_0cfb, 0x05ca_c729, 0x3c7a_f4b5),
    keys("com.apple.driver.AppleT6002CLPC",   0xb501_816b, 0x9e2c_3e8b, 0x14d5_a574, 0x7670_4b08, 0x2d7b_7fb2, 0xe16c_1380),
    keys("com.apple.driver.AppleT8112CLPC",   0xb543_5137, 0x638d_9d52, 0xea08_9c36, 0x8b6e_0faa, 0x9a6e_145a, 0x824b_cad4),
    keys("com.apple.driver.AppleT6020CLPC",   0xe301_cd75, 0x8f34_cc7a, 0x7415_fb0f, 0x2624_4bb6, 0x5f31_a20f, 0x9877_104c),
    keys("com.apple.driver.AppleT6022CLPC",   0x76f5_fd21, 0x9062_4499, 0x3c09_7307, 0xe8f2_ddf9, 0xfa86_5a91, 0xef56_ddf3),
    keys("com.apple.driver.AppleT8122CLPC",   0x54b5_1c99, 0xce2d_d9dc, 0x960a_fe2f, 0x0b96_41d3, 0x0d80_2b66, 0x9709_cb31),
    keys("com.apple.driver.AppleT6030CLPC",   0xbe03_a01a, 0x472d_6c9c, 0xb899_0c38, 0x2380_9c0f, 0xb594_4c6a, 0xd9ac_8450),
    keys("com.apple.driver.AppleT6031CLPC",   0xd81e_0084, 0x1085_f98b, 0x313a_3be1, 0x65ff_699d, 0x10cb_1f6a, 0xfc3f_ce99),
    keys("com.apple.driver.AppleT6032CLPC",   0xf4a2_1308, 0x85e8_0513, 0x63dc_8e25, 0x63dc_aae5, 0x8cc1_3c13, 0xfaa8_ecc3),
    keys("com.apple.driver.AppleT8132CLPC",   0x4a85_4d94, 0x3b3e_3b79, 0x1ff4_a9ec, 0x9a44_d225, 0xe340_48d2, 0x6ab9_90d0),
    keys("com.apple.driver.AppleT6041CLPC",   0x263c_f1f0, 0x22e4_cf59, 0x7849_fb5c, 0xfe93_2624, 0x8fc8_575c, 0xf708_48a3),
    keys("com.apple.driver.AppleT8140CLPC",   0x9bf5_4436, 0xef70_589c, 0x569d_941b, 0x035f_5005, 0x5344_2391, 0x7f14_616c),
    keys("com.apple.driver.AppleT8142CLPC",   0x4370_8744, 0x6a01_0933, 0xbc5e_fdf0, 0xe136_33aa, 0x2b8d_7fec, 0xa65a_b9a8),
    Keys {
        // M5 Pro / M5 Max. Clusters: Performance 0, Performance 1, Super.
        clusters: Some((0xa451_4996, 3)),
        ..keys("com.apple.driver.AppleT6050CLPC", 0xd3ab_60bb, 0x27cd_4ce5, 0x8c0f_1c59, 0xf3c8_1737, 0x3d89_affa, 0x958c_fda5)
    },
    keys("com.apple.driver.AppleT6050dCLPC",  0x18b3_010e, 0xedec_a11a, 0x188b_d883, 0xa331_96f9, 0x0b3c_717e, 0x9ee7_9a95),
    keys("com.apple.driver.AppleT8152CLPC",   0x485d_4bfc, 0x1178_4654, 0x9380_c6bb, 0xe17c_13bf, 0x4e80_dc41, 0xf6ed_063b),
];

const fn report_id(index: u32, key: u32) -> u64 {
    ((index as u64) << 32) | key as u64
}

/// Energy reports of the CLPC driver `bundle` (its `CFBundleIdentifier`) on
/// macOS `os_major`. `None` for unknown drivers and OS versions: indices move
/// between major releases, and a wrong ID would read an unrelated counter.
pub fn energy_reports(bundle: &str, os_major: u32) -> Option<Vec<ClpcReport>> {
    if os_major != 27 {
        return None;
    }
    let k = KEYS.iter().find(|k| k.bundle == bundle)?;
    let ix = &INDICES_27;
    let scalar = |kind, index, key| ClpcReport {
        kind,
        id: report_id(index, key),
        format: FORMAT_SIMPLE,
        values: 1,
    };
    let mut reports = vec![
        scalar(ClpcKind::Cpu, ix.cpu, k.cpu),
        scalar(ClpcKind::Gpu, ix.gpu, k.gpu),
        scalar(ClpcKind::Ane, ix.ane, k.ane),
        scalar(ClpcKind::Dram, ix.ram, k.ram),
        scalar(ClpcKind::Dcs, ix.dcs, k.dcs),
        scalar(ClpcKind::Package, ix.chip, k.chip),
    ];
    if let Some((key, values)) = k.clusters {
        reports.push(ClpcReport {
            kind: ClpcKind::Clusters,
            id: report_id(ix.clusters, key),
            format: FORMAT_SIMPLE_ARRAY,
            values,
        });
    }
    Some(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m5_max_reports_use_verified_ids() {
        let r = energy_reports("com.apple.driver.AppleT6050CLPC", 27).unwrap();
        let id = |kind| r.iter().find(|x| x.kind == kind).unwrap().id;
        assert_eq!(id(ClpcKind::Cpu), 0x0000_0010_d3ab_60bb);
        assert_eq!(id(ClpcKind::Gpu), 0x0000_0019_27cd_4ce5);
        assert_eq!(id(ClpcKind::Ane), 0x0000_0018_8c0f_1c59);
        assert_eq!(id(ClpcKind::Dram), 0x0000_001a_f3c8_1737);
        assert_eq!(id(ClpcKind::Dcs), 0x0000_001b_3d89_affa);
        assert_eq!(id(ClpcKind::Package), 0x0000_0009_958c_fda5);
        assert_eq!(id(ClpcKind::Clusters), 0x0000_0011_a451_4996);
    }

    #[test]
    fn cluster_array_is_a_one_element_simple_array() {
        let r = energy_reports("com.apple.driver.AppleT6050CLPC", 27).unwrap();
        let c = r.iter().find(|x| x.kind == ClpcKind::Clusters).unwrap();
        assert_eq!(c.values, 3);
        assert_eq!(c.elements(), 1);
        assert_eq!(c.channel_type(), 4 | (2 << 16) | (1 << 32));
        let cpu = r.iter().find(|x| x.kind == ClpcKind::Cpu).unwrap();
        assert_eq!(cpu.channel_type(), 1 | (2 << 16) | (1 << 32));
    }

    #[test]
    fn drivers_without_a_known_cluster_key_get_scalars_only() {
        let r = energy_reports("com.apple.driver.AppleT8103CLPCv3", 27).unwrap();
        assert_eq!(r.len(), 6);
        assert!(r.iter().all(|x| x.kind != ClpcKind::Clusters));
    }

    #[test]
    fn unknown_driver_or_os_has_no_reports() {
        assert!(energy_reports("com.apple.driver.AppleT9999CLPC", 27).is_none());
        for os in [0, 15, 26, 28] {
            assert!(energy_reports("com.apple.driver.AppleT6050CLPC", os).is_none());
        }
    }

    #[test]
    fn table_has_no_duplicates_and_names_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for k in KEYS {
            assert!(seen.insert(k.bundle), "duplicate driver {}", k.bundle);
            let r = energy_reports(k.bundle, 27).unwrap();
            let ids: std::collections::HashSet<u64> = r.iter().map(|x| x.id).collect();
            assert_eq!(ids.len(), r.len());
            for x in &r {
                assert_eq!(kind_from_name(x.name()), Some(x.kind));
            }
        }
    }
}
