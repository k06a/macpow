//! Hardware checks for the display fields: what macpow reports is compared
//! with an independent macOS source on the machine running the test (`ioreg`,
//! `system_profiler`), not with macpow's own FFI readers.
//!
//! Each check passes with a "skipped" note when its source has nothing to
//! compare against: no built-in panel, display asleep, virtual machine, or a
//! system language other than English for `system_profiler`.

use macpow::metrics;
use macpow::types::DisplayInfo;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayIsBuiltin(display: u32) -> i32;
}

fn main_display_is_builtin() -> bool {
    // SAFETY: both functions take and return plain integers and accept any
    // display ID, including one that is no longer online.
    unsafe { CGDisplayIsBuiltin(CGMainDisplayID()) != 0 }
}

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

/// Level the built-in panel is driven at, in nits, as printed by `ioreg` for
/// the framebuffer below `disp0` (16.16 fixed point in the registry).
fn ioreg_panel_nits() -> Option<f32> {
    let out = Command::new("/usr/sbin/ioreg")
        .args(["-r", "-n", "disp0", "-d", "2", "-l", "-w", "0"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let raw: i64 = text
        .lines()
        .find_map(|line| {
            line.trim_start_matches([' ', '|'])
                .strip_prefix("\"IOMFBBrightnessLevel\" = ")
        })?
        .trim()
        .parse()
        .ok()?;
    Some(raw as f32 / 65536.0)
}

/// Text after "Resolution:" for the display that `system_profiler` marks as
/// main, e.g. "3024 x 1964 Retina".
///
/// For an Apple panel this is a constant looked up by the panel's product ID,
/// so it names the physical panel whatever scaling is selected. (For other
/// displays it follows the current mode.)
fn system_profiler_main_resolution() -> Option<String> {
    let out = Command::new("/usr/sbin/system_profiler")
        .arg("SPDisplaysDataType")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut resolution = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Resolution: ") {
            resolution = Some(rest.to_string());
        } else if line == "Main Display: Yes" {
            return resolution;
        }
    }
    None
}

/// `system_profiler` now and then answers without any display section (seen
/// in about one run in eight, always an instant reply), so ask a few times.
fn main_resolution() -> Option<String> {
    (0..4).find_map(|_| system_profiler_main_resolution())
}

/// The "W x H" pair inside a resolution string of `system_profiler`. The pair
/// is not always in front ("Retina 4.5K (4480 x 2520)") and other numbers can
/// surround it, so it is located by its " x ".
fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let (before, after) = text.split_once(" x ")?;
    let width = before.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    let height = after.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

#[test]
fn resolution_is_read_from_every_system_profiler_format() {
    // Strings from the Localizable.loctable of SPDisplaysReporter.
    for (text, expected) in [
        ("3024 x 1964 Retina", (3024, 1964)),
        ("Retina 4.5K (4480 x 2520)", (4480, 2520)),
        ("Retina 5K (5120 x 2880)", (5120, 2880)),
        ("6016 x 3384 Retina 6K", (6016, 3384)),
        (
            "3840 x 2160 (2160p/4K UHD 1 - Ultra High Definition)",
            (3840, 2160),
        ),
        (
            "1920 x 1080 @ 60Hz (1080p FHD - Full High Definition)",
            (1920, 1080),
        ),
        ("704 x 480 60Hz (480i)", (704, 480)),
    ] {
        assert_eq!(parse_resolution(text), Some(expected), "{text:?}");
    }
    for text in ["", "Retina", "Retina 5K", "3024 by 1964", "wide x tall"] {
        assert_eq!(parse_resolution(text), None, "{text:?}");
    }
}

#[test]
fn nits_match_the_level_the_panel_is_driven_at() {
    let _guard = ONE_SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if !main_display_is_builtin() {
        eprintln!("skipped: the main display is not a built-in panel");
        return;
    }
    // Auto-brightness and display sleep can move the level while we look, so
    // read the registry on both sides of the sample and accept the span.
    let before = ioreg_panel_nits();
    let display = sample_display();
    let after = ioreg_panel_nits();
    let (Some(before), Some(display), Some(after)) = (before, display, after) else {
        eprintln!("skipped: no panel level in the registry or no display in the sampler");
        return;
    };
    let (low, high) = (before.min(after), before.max(after));
    let slack = 0.02 * high + 0.5;
    eprintln!(
        "macpow {:.2} nits at {:.1} % slider; registry {before:.2} .. {after:.2} nits",
        display.nits, display.brightness_pct
    );
    assert!(
        display.nits >= low - slack && display.nits <= high + slack,
        "macpow reports {} nits but the panel is driven at {before} .. {after} nits \
         (slider {} %, peak {} nits)",
        display.nits,
        display.brightness_pct,
        display.peak_nits
    );
}

#[test]
fn width_and_height_are_the_panel_pixels() {
    let _guard = ONE_SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(display) = sample_display() else {
        eprintln!("skipped: no display in the sampler");
        return;
    };
    // system_profiler lists no display at all while the display sleeps, and
    // its labels are translated when the system language is not English.
    let Some(resolution) = main_resolution() else {
        eprintln!(
            "skipped: system_profiler printed no \"Resolution:\" line followed by \
             \"Main Display: Yes\""
        );
        return;
    };
    let Some(expected) = parse_resolution(&resolution) else {
        eprintln!("skipped: no \"W x H\" in system_profiler's resolution {resolution:?}");
        return;
    };
    eprintln!(
        "macpow {} x {}; system_profiler {resolution:?}",
        display.width_px, display.height_px
    );
    // Not the size in points (half of this on a Retina mode) and not the
    // rendered size of a scaled mode (3600 x 2338 for "More Space" on a
    // 3024 x 1964 panel).
    assert_eq!((display.width_px, display.height_px), expected);
}
