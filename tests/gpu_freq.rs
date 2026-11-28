//! Hardware test: the GPU frequency table is the one in the device tree's
//! `sgx` node, as an independent reader (`ioreg`) shows it.
//!
//! On an M5 Max the pmgr `voltage-states*` guess used to pick a 26-entry
//! table that isn't the GPU's, so an idle GPU read 732 MHz instead of 338.

use macpow::ioreport::IOReportSampler;
use std::process::Command;

/// A `<hex>` data property of the `sgx` node, from `ioreg`'s text output.
fn sgx_property(ioreg: &str, key: &str) -> Option<Vec<u8>> {
    let needle = format!("\"{key}\" = <");
    let start = ioreg.find(&needle)? + needle.len();
    let hex = &ioreg[start..start + ioreg[start..].find('>')?];
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[test]
fn gpu_table_matches_device_tree_sgx_node() {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-r", "-n", "sgx", "-w0", "-l"])
        .output()
        .expect("ioreg should run");
    let ioreg = String::from_utf8_lossy(&out.stdout);
    let (Some(states), Some(count)) = (
        sgx_property(&ioreg, "perf-states"),
        sgx_property(&ioreg, "gpu-num-perf-states"),
    ) else {
        eprintln!("no sgx perf-states on this machine: nothing to compare with");
        return;
    };

    // Every voltage bin repeats the same frequencies; the 0 Hz entry is "off".
    let mut expected: Vec<u32> = states
        .chunks(8)
        .filter(|pair| pair.len() == 8)
        .map(|pair| le_u32(pair) / 1_000_000)
        .filter(|&mhz| mhz > 0)
        .collect();
    expected.sort_unstable();
    expected.dedup();
    if expected.len() != le_u32(&count) as usize {
        // A fact about this Mac's device tree, not about macpow, which doesn't
        // trust such a node either and keeps to its guess among the pmgr tables.
        eprintln!("sgx perf-states don't hold gpu-num-perf-states frequencies: skipping");
        return;
    }

    let sampler = IOReportSampler::new().unwrap();
    assert_eq!(
        sampler.gpu_freqs, expected,
        "GPU DVFS table should be the sgx node's"
    );

    let s1 = sampler.sample().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let s2 = sampler.sample().unwrap();
    let mhz = sampler.parse_power(&s1, &s2).unwrap().gpu_freq_mhz;
    // 0 = the GPU was powered off for the whole interval.
    if mhz > 0 {
        let (min, max) = (expected[0], expected[expected.len() - 1]);
        assert!(
            (min..=max).contains(&mhz),
            "gpu_freq_mhz {mhz} outside the GPU's range {min}..={max}"
        );
    }
}
