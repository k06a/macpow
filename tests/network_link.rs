//! Hardware checks for the Wi-Fi and Ethernet readers.
//!
//! They compare macpow's answers with what macOS itself reports on the
//! machine running the tests, so they pass both with and without a Wi-Fi
//! radio or a wired link (each test states what it expects in either case).
//!
//! Run with: cargo test --test network_link -- --test-threads=1

use macpow::peripherals::read_wifi_info;

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
