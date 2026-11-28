//! The SMC-independent system metrics, checked against what macOS's own tools
//! report on the machine that runs the tests.

use macpow::metrics;
use std::process::Command;

fn stdout_of(program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The number on a `vm_stat` line such as `Pages wired down:      478201.`
fn vm_stat_pages(vm_stat: &str, label: &str) -> u64 {
    vm_stat
        .lines()
        .find_map(|line| line.strip_prefix(label)?.strip_prefix(':'))
        .and_then(|value| value.trim().trim_end_matches('.').parse().ok())
        .unwrap_or_else(|| panic!("no {label:?} line in vm_stat output:\n{vm_stat}"))
}

#[test]
fn mem_used_is_activity_monitors_memory_used() {
    const GIB: f64 = (1u64 << 30) as f64;
    let vm_stat = stdout_of("/usr/bin/vm_stat", &[]);
    let ours = metrics::read_mem_used_gb() as f64;

    // First line: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size: u64 = vm_stat
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next()?.parse().ok())
        .expect("page size in the vm_stat header");
    let pages = |label| vm_stat_pages(&vm_stat, label);
    let used_pages = pages("Anonymous pages").saturating_sub(pages("Pages purgeable"))
        + pages("Pages wired down")
        + pages("Pages occupied by compressor");
    let reference = (used_pages * page_size) as f64 / GIB;

    let total: f64 = stdout_of("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .trim()
        .parse()
        .expect("hw.memsize");
    let total = total / GIB;

    assert!(ours > 0.5 && ours <= total, "{ours} GiB of {total} GiB");
    // The two readings are a few milliseconds apart; the file cache that the
    // figure must not include is far larger than this on a Mac in use.
    let tolerance = (0.03 * total).max(1.0);
    assert!(
        (ours - reference).abs() <= tolerance,
        "mem_used_gb {ours:.2} vs {reference:.2} GiB from vm_stat"
    );
}
