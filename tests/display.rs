//! Hardware checks for the display fields: what macpow reports is compared
//! with an independent macOS source on the machine running the test (`ioreg`,
//! `system_profiler`), not with macpow's own FFI readers.
//!
//! Each check passes with a "skipped" note when its source has nothing to
//! compare against: no built-in panel, display asleep, virtual machine.

use macpow::metrics;
use macpow::types::DisplayInfo;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Samplers start a dozen threads each; run one at a time whatever
/// `--test-threads` says.
static ONE_SAMPLER: Mutex<()> = Mutex::new(());

/// Display fields of a fresh sampler, once its display thread has published
/// them. `None` if no display with a brightness control shows up.
fn sample_display() -> Option<DisplayInfo> {
    let sampler = metrics::Sampler::new(200);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let display = sampler.snapshot().display;
        if display.available {
            return Some(display);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// "Resolution: W x H" of the display that `system_profiler` marks as main.
fn system_profiler_main_resolution() -> Option<(u32, u32)> {
    let out = Command::new("/usr/sbin/system_profiler")
        .arg("SPDisplaysDataType")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut resolution = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Resolution: ") {
            // "3024 x 1964 Retina": the first two numbers.
            let mut numbers = rest.split_whitespace().filter_map(|w| w.parse().ok());
            resolution = numbers.next().zip(numbers.next());
        } else if line == "Main Display: Yes" {
            return resolution;
        }
    }
    None
}

/// `system_profiler` now and then answers without any display section (seen
/// in about one run in eight, always an instant reply), so ask a few times.
fn main_resolution() -> Option<(u32, u32)> {
    (0..4).find_map(|_| system_profiler_main_resolution())
}

#[test]
fn width_and_height_are_pixels_not_points() {
    let _guard = ONE_SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(display) = sample_display() else {
        eprintln!("skipped: no display in the sampler");
        return;
    };
    // system_profiler lists no display at all while the display sleeps.
    let Some(expected) = main_resolution() else {
        eprintln!("skipped: system_profiler reports no main display");
        return;
    };
    eprintln!(
        "macpow {} x {}; system_profiler {} x {}",
        display.width_px, display.height_px, expected.0, expected.1
    );
    assert_eq!((display.width_px, display.height_px), expected);
}
