//! Hardware test: the summed USB power-out of the live sampler. Passes on any
//! Mac, with or without a device drawing power from a port.

use macpow::metrics::Sampler;
use std::time::{Duration, Instant};

/// With no port sourcing power the total used to be the float sum of an empty
/// list, which is -0.0 and reached `--json` as `"usb_power_out_w": -0.0`.
#[test]
fn usb_power_out_is_non_negative_sum_of_ports() {
    let sampler = Sampler::new(100);
    // The USB thread publishes on its own schedule and nothing signals its
    // first round, so watch it for a while instead of taking one snapshot.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let m = sampler.snapshot();
        assert!(
            m.usb_power_out_w >= 0.0 && m.usb_power_out_w.is_sign_positive(),
            "usb_power_out_w must be +0.0 or more, got {:?}",
            m.usb_power_out_w
        );
        let per_port: f32 = m.usb_power_per_port.iter().map(|p| p.power_w).sum();
        assert!(
            (m.usb_power_out_w - per_port).abs() < 1e-3,
            "usb_power_out_w {} differs from the per-port sum {}",
            m.usb_power_out_w,
            per_port
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
