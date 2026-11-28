//! Hardware test for the battery values whose registry location depends on the
//! macOS release (macOS 27 moved them into nested dictionaries). It fails when
//! `read_battery` no longer finds them on the machine running the test.

use macpow::battery;

#[test]
fn battery_gauge_values_are_found() {
    let b = battery::read_battery();
    // Desktops have no AppleSmartBattery, or a phantom one that reports zeros.
    if !b.present || b.max_capacity == 0 {
        return;
    }

    assert!(
        b.design_capacity_mah >= 2000.0 && b.design_capacity_mah <= 12000.0,
        "design capacity {} mAh not found or unreasonable",
        b.design_capacity_mah
    );
    assert!(
        b.max_capacity_mah >= 0.3 * b.design_capacity_mah
            && b.max_capacity_mah <= 1.15 * b.design_capacity_mah,
        "full-charge capacity {} mAh does not fit design capacity {} mAh",
        b.max_capacity_mah,
        b.design_capacity_mah
    );
    assert!(
        b.health_pct >= 30.0 && b.health_pct <= 115.0,
        "health {} % not found or unreasonable",
        b.health_pct
    );
    assert!(
        b.capacity_wh >= 15.0 && b.capacity_wh <= 120.0,
        "capacity {} Wh not found or unreasonable",
        b.capacity_wh
    );
    assert!(
        b.temperature_c > 0.0 && b.temperature_c < 70.0,
        "temperature {} °C not found or unreasonable",
        b.temperature_c
    );
}
