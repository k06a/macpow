//! Hardware checks for the Wi-Fi and Ethernet readers.
//!
//! They compare macpow's answers with what macOS itself reports on the
//! machine running the tests, so they pass both with and without a Wi-Fi
//! radio or a wired link (each test states what it expects in either case).
//!
//! Run with: cargo test --test network_link -- --test-threads=1

use macpow::peripherals::{read_ethernet_info, read_wifi_info, WifiReader};
use std::process::Command;

/// Names of all `en*` interfaces, from `ifconfig -l`.
fn en_interfaces() -> Vec<String> {
    let out = Command::new("/sbin/ifconfig")
        .arg("-l")
        .output()
        .expect("run /sbin/ifconfig -l");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter(|name| name.starts_with("en"))
        .map(str::to_string)
        .collect()
}

/// Whether `ifconfig <iface>` prints "status: active", i.e. the link has carrier.
fn ifconfig_says_active(iface: &str) -> bool {
    let out = Command::new("/sbin/ifconfig")
        .arg(iface)
        .output()
        .expect("run /sbin/ifconfig <iface>");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == "status: active")
}

/// `read_ethernet_info(excluded)` must report a link exactly when some `en*`
/// interface other than `excluded` has carrier, and must then name one of them.
fn assert_ethernet_matches_ifconfig(excluded: &str) {
    let eth = read_ethernet_info(excluded);
    let with_carrier: Vec<String> = en_interfaces()
        .into_iter()
        .filter(|name| name != excluded && ifconfig_says_active(name))
        .collect();
    assert_eq!(
        eth.connected,
        !with_carrier.is_empty(),
        "excluding {excluded:?}: macpow says {eth:?}, ifconfig has carrier on {with_carrier:?}"
    );
    if eth.connected {
        assert!(
            with_carrier.contains(&eth.interface_name),
            "{} has no carrier; ifconfig has carrier on {with_carrier:?}",
            eth.interface_name
        );
    } else {
        assert!(eth.interface_name.is_empty());
        assert_eq!(eth.link_speed_mbps, 0);
    }
}

#[test]
fn wifi_connected_follows_the_link() {
    let wifi = read_wifi_info();
    if wifi.interface_name.is_empty() {
        // No Wi-Fi hardware, e.g. in a VM.
        assert!(!wifi.connected);
        return;
    }
    // Whether the network name is known must not matter: macOS withholds it
    // from processes without Location permission.
    assert_eq!(
        wifi.connected,
        ifconfig_says_active(&wifi.interface_name),
        "connected must equal `ifconfig {}` saying \"status: active\"",
        wifi.interface_name
    );
}

#[test]
fn wifi_ssid_is_a_name_or_empty() {
    let wifi = read_wifi_info();
    assert_ne!(wifi.ssid, "<redacted>", "the placeholder is not a name");
    if !wifi.connected {
        assert!(wifi.ssid.is_empty());
    }
}

#[test]
fn ethernet_is_connected_only_with_carrier() {
    // Idle Thunderbolt and USB-C (NCM) ports are UP and RUNNING without
    // carrier; they must not show up as an Ethernet link.
    assert_ethernet_matches_ifconfig(&read_wifi_info().interface_name);
}

#[test]
fn carrier_is_found_on_an_active_link() {
    // With nothing excluded the Wi-Fi interface is a candidate as well, which
    // covers the "has carrier" answer on a machine without a wired link.
    assert_ethernet_matches_ifconfig("");
}

#[test]
fn wifi_tx_rate_is_a_real_rate() {
    let wifi = read_wifi_info();
    if wifi.connected {
        // 1 Mbps is the slowest 802.11 rate. A `double` read from the wrong
        // register shows up here as NaN or as a denormal that rounds to 0.
        assert!(
            (1.0..=10_000.0).contains(&wifi.tx_rate_mbps),
            "tx_rate_mbps = {} on a connected link",
            wifi.tx_rate_mbps
        );
    } else {
        assert_eq!(wifi.tx_rate_mbps, 0.0);
    }
}

/// Memory this process is charged for, in bytes (Activity Monitor's "Memory").
fn phys_footprint() -> u64 {
    // SAFETY: an all-zero `rusage_info_v2` is a valid value, and it is the
    // buffer layout that RUSAGE_INFO_V2 asks the kernel to fill in.
    unsafe {
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let rc = libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
        );
        assert_eq!(rc, 0, "proc_pid_rusage failed");
        info.ri_phys_footprint
    }
}

#[test]
fn wifi_reads_do_not_leak() {
    // A test thread is like a sampler thread: nobody drains autoreleased
    // Objective-C objects for it.
    let mut reader = WifiReader::default();
    // One-time costs first: the XPC connection, selector and class caches.
    for _ in 0..50 {
        reader.read();
    }

    // A read takes about 10 ms, so the loop is kept short.
    const READS: u64 = 400;
    let before = phys_footprint();
    for _ in 0..READS {
        reader.read();
    }
    let grown = phys_footprint().saturating_sub(before);
    // Without a pool a read leaks about 1.7 KB of CoreWLAN temporaries;
    // with one the footprint moves by a few pages at most.
    assert!(
        grown < READS * 400,
        "footprint grew by {grown} bytes over {READS} reads"
    );
}
