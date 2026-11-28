//! Static machine information is read in-process (sysctlbyname, IORegistry).
//! These tests compare it with what the system tools print on the same Mac.
//! They need Apple Silicon hardware, like tests/integration.rs.
//!
//! Run with: cargo test --test static_info

use std::process::Command;

fn tool(path: &str, args: &[&str]) -> String {
    let out = Command::new(path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(out.status.success(), "{path} {args:?} failed");
    String::from_utf8(out.stdout).expect("utf-8 output")
}

fn sysctl_tool(name: &str) -> String {
    tool("/usr/sbin/sysctl", &["-n", name])
        .trim_end_matches('\n')
        .to_string()
}

#[test]
fn sysctl_strings_match_the_sysctl_tool() {
    // The two names the TUI builds its machine name from.
    for name in ["machdep.cpu.brand_string", "hw.model"] {
        assert_eq!(
            macpow::topology::sysctl_string(name),
            Some(sysctl_tool(name)),
            "{name}"
        );
    }
}

#[test]
fn sysctl_memsize_matches_the_sysctl_tool() {
    let expected: u64 = sysctl_tool("hw.memsize").parse().expect("a number");
    assert_eq!(macpow::topology::sysctl_u64("hw.memsize"), Some(expected));
}

#[test]
fn sampler_static_fields_match_system_tools_without_a_usable_path() {
    let memsize: u64 = sysctl_tool("hw.memsize").parse().expect("a number");
    // `      "gpu-core-count" = 40`
    let accelerators = tool("/usr/sbin/ioreg", &["-r", "-c", "IOAccelerator", "-d", "1"]);
    let gpu_cores: u32 = accelerators
        .lines()
        .find_map(|line| line.trim().strip_prefix("\"gpu-core-count\" = "))
        .expect("an accelerator with gpu-core-count (Apple Silicon)")
        .parse()
        .expect("a number");
    assert!(gpu_cores > 0);

    // The static fields used to come from `sysctl` and `ioreg` found through
    // PATH, and read 0 wherever /usr/sbin is not on it (launchd, cron). PATH
    // is process-wide; the other tests here only run tools by absolute path.
    std::env::set_var("PATH", "/nonexistent");
    let sampler = macpow::metrics::Sampler::new(500);
    let m = sampler.snapshot();

    assert_eq!(u64::from(m.dram_gb), memsize >> 30, "dram_gb");
    assert_eq!(m.gpu_cores, gpu_cores, "gpu_cores");
}
