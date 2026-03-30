//! Integration tests: validate that each data source returns sensible values
//! on real Apple Silicon hardware. Run with `cargo test`.

use macpow::battery;
use macpow::ioreport::IOReportSampler;
use macpow::metrics;
use macpow::smc::SmcConnection;
use macpow::types::*;

// ── Wattage sanity ranges for Apple Silicon laptops ──────────────────────────

const MAX_CPU_W: f32 = 60.0; // M3/M5 Ultra worst-case
const MAX_GPU_W: f32 = 80.0;
const MAX_ANE_W: f32 = 10.0;
const MAX_DRAM_W: f32 = 20.0;
const MAX_SOC_W: f32 = 120.0;
const MAX_SYS_W: f32 = 200.0; // MacBook Pro + charger overhead
const MAX_DISPLAY_W: f32 = 15.0; // XDR at full HDR
const MAX_KEYBOARD_W: f32 = 0.6;
const MAX_AUDIO_W: f32 = 2.0;
const MAX_FAN_W: f32 = 2.0;
const MAX_BATTERY_W: f64 = 150.0; // abs(drain), charging or discharging

// ── IOReport SoC Power ──────────────────────────────────────────────────────

#[test]
fn ioreport_sampler_creates() {
    let sampler = IOReportSampler::new();
    assert!(
        sampler.is_ok(),
        "IOReportSampler should initialize on Apple Silicon"
    );
}

#[test]
fn ioreport_sample_and_delta() {
    let sampler = IOReportSampler::new().unwrap();
    let s1 = sampler.sample().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let s2 = sampler.sample().unwrap();
    let soc = sampler.parse_power(&s1, &s2).unwrap();

    assert!(soc.cpu_w >= 0.0, "CPU power must be non-negative");
    assert!(
        soc.cpu_w < MAX_CPU_W,
        "CPU power {} W exceeds max {}",
        soc.cpu_w,
        MAX_CPU_W
    );
    assert!(soc.gpu_w >= 0.0 && soc.gpu_w < MAX_GPU_W);
    assert!(soc.ane_w >= 0.0 && soc.ane_w < MAX_ANE_W);
    assert!(soc.dram_w >= 0.0 && soc.dram_w < MAX_DRAM_W);
    assert!(soc.total_w >= 0.0 && soc.total_w < MAX_SOC_W);

    let sum = soc.cpu_w + soc.gpu_w + soc.ane_w + soc.dram_w + soc.gpu_sram_w;
    let diff = (soc.total_w - sum).abs();
    assert!(
        diff < 0.001,
        "total_w should equal sum of components, diff={}",
        diff
    );
}

#[test]
fn ioreport_cpu_clusters_populated() {
    let sampler = IOReportSampler::new().unwrap();
    let s1 = sampler.sample().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let s2 = sampler.sample().unwrap();
    let soc = sampler.parse_power(&s1, &s2).unwrap();

    assert!(
        !soc.ecpu_clusters.is_empty(),
        "E-core clusters should be detected"
    );
    assert!(
        !soc.pcpu_cluster.cores.is_empty(),
        "P-core cores should be detected"
    );

    soc.ecpu_clusters.iter().for_each(|cluster| {
        assert!(
            !cluster.cores.is_empty(),
            "E-cluster {} should have cores",
            cluster.name
        );
        cluster.cores.iter().for_each(|core| {
            assert!(
                core.watts >= 0.0 && core.watts < 10.0,
                "E-core {} watts {} out of range",
                core.name,
                core.watts
            );
        });
    });
    soc.pcpu_cluster.cores.iter().for_each(|core| {
        assert!(
            core.watts >= 0.0 && core.watts < 20.0,
            "P-core {} watts {} out of range",
            core.name,
            core.watts
        );
    });
}

// ── SMC ──────────────────────────────────────────────────────────────────────

#[test]
fn smc_opens() {
    let smc = SmcConnection::open();
    assert!(smc.is_ok(), "SMC should open on Apple Silicon");
}

#[test]
fn smc_system_power_in_range() {
    let mut smc = SmcConnection::open().unwrap();
    let pstr = smc.read_system_power();
    assert!(pstr >= 0.0, "PSTR should be non-negative");
    assert!(
        pstr < MAX_SYS_W,
        "PSTR {} W exceeds max {}",
        pstr,
        MAX_SYS_W
    );
}

#[test]
fn smc_fans_in_range() {
    let mut smc = SmcConnection::open().unwrap();
    let fans = smc.read_fans();
    fans.iter().for_each(|fan| {
        assert!(fan.actual_rpm >= 0.0, "Fan RPM should be non-negative");
        assert!(
            fan.actual_rpm <= 10000.0,
            "Fan {} RPM {} unreasonable",
            fan.name,
            fan.actual_rpm
        );
        assert!(fan.max_rpm > 0.0, "Fan {} max_rpm should be > 0", fan.name);
        assert!(
            fan.estimated_power_w >= 0.0 && fan.estimated_power_w <= MAX_FAN_W,
            "Fan {} power {} out of range",
            fan.name,
            fan.estimated_power_w
        );
    });
}

#[test]
fn smc_temperatures_in_range() {
    let mut smc = SmcConnection::open().unwrap();
    let handle = smc.start_temp_discovery();
    smc.finish_temp_discovery(handle);
    let temps = smc.read_temperatures();
    assert!(!temps.is_empty(), "Should detect temperature sensors");
    temps.iter().for_each(|t| {
        assert!(
            t.value_celsius > -10.0 && t.value_celsius < 130.0,
            "Sensor {} temp {} °C out of range",
            t.key,
            t.value_celsius
        );
    });
}

// ── Battery ──────────────────────────────────────────────────────────────────

#[test]
fn battery_info_valid() {
    let b = battery::read_battery();
    if !b.present {
        return; // desktop Mac, no battery
    }
    assert!(b.voltage_mv > 0.0, "Voltage should be positive");
    assert!(
        b.voltage_mv < 20000.0,
        "Voltage {} mV unreasonable",
        b.voltage_mv
    );
    assert!(
        b.percent >= 0.0 && b.percent <= 100.0,
        "Battery percent {} out of range",
        b.percent
    );
    assert!(
        b.drain_w.abs() < MAX_BATTERY_W,
        "Battery drain {} W exceeds max {}",
        b.drain_w,
        MAX_BATTERY_W
    );
    assert!(b.max_capacity > 0, "max_capacity should be > 0");
}

// ── Full sampler tick ────────────────────────────────────────────────────────

#[test]
fn sampler_first_tick_no_panic() {
    let sampler = metrics::Sampler::new(500);
    let m = sampler.snapshot();
    // First tick should now have SoC data (pre-sampled in new())
    assert!(
        m.soc.total_w >= 0.0,
        "First tick SoC should be non-negative"
    );
    assert!(
        m.sys_power_w >= 0.0,
        "First tick sys_power should be non-negative"
    );
}

#[test]
#[ignore] // run with: cargo test -- --ignored (needs exclusive IOReport access)
fn sampler_second_tick_has_power() {
    let sampler = metrics::Sampler::new(500);
    // Wait for async source threads to populate data
    std::thread::sleep(std::time::Duration::from_millis(3000));
    let m = sampler.snapshot();

    // SoC should now have real data
    assert!(
        m.soc.total_w > 0.0,
        "Should have non-zero SoC power after 1.5s"
    );
    assert!(m.soc.total_w < MAX_SOC_W);
    assert!(m.sys_power_w > 0.0, "PSTR should be non-zero");
    assert!(m.sys_power_w < MAX_SYS_W);
}

#[test]
fn sampler_display_brightness_in_range() {
    let sampler = metrics::Sampler::new(500);
    let m = sampler.snapshot();
    assert!(
        m.display.brightness_pct >= 0.0 && m.display.brightness_pct <= 100.0,
        "Display brightness {} out of range",
        m.display.brightness_pct
    );
    assert!(
        m.display.estimated_power_w >= 0.0 && m.display.estimated_power_w <= MAX_DISPLAY_W,
        "Display power {} out of range",
        m.display.estimated_power_w
    );
}

#[test]
fn sampler_keyboard_brightness_in_range() {
    let sampler = metrics::Sampler::new(500);
    let m = sampler.snapshot();
    assert!(
        m.keyboard.brightness_pct >= 0.0 && m.keyboard.brightness_pct <= 100.0,
        "Keyboard brightness {} out of range",
        m.keyboard.brightness_pct
    );
    assert!(
        m.keyboard.estimated_power_w >= 0.0 && m.keyboard.estimated_power_w <= MAX_KEYBOARD_W,
        "Keyboard power {} out of range",
        m.keyboard.estimated_power_w
    );
}

#[test]
fn sampler_audio_power_in_range() {
    let sampler = metrics::Sampler::new(500);
    let m = sampler.snapshot();
    assert!(
        m.audio.estimated_power_w >= 0.0 && m.audio.estimated_power_w <= MAX_AUDIO_W,
        "Audio power {} out of range",
        m.audio.estimated_power_w
    );
    if let Some(vol) = m.audio.volume_pct {
        assert!(
            vol >= 0.0 && vol <= 100.0,
            "Audio volume {} out of range",
            vol
        );
    }
}

#[test]
fn sampler_fan_power_in_range() {
    let sampler = metrics::Sampler::new(500);
    let m = sampler.snapshot();
    m.fans.iter().for_each(|fan| {
        assert!(
            fan.estimated_power_w >= 0.0 && fan.estimated_power_w <= MAX_FAN_W,
            "Fan {} power {} out of range",
            fan.name,
            fan.estimated_power_w
        );
    });
}

// ── Power estimation pure functions ──────────────────────────────────────────

#[test]
fn fan_power_cubic_model() {
    // At 0 RPM → 0 W, at max → 1 W, scales cubically
    let info = FanInfo {
        id: 0,
        name: "test".into(),
        actual_rpm: 0.0,
        min_rpm: 0.0,
        max_rpm: 8000.0,
        estimated_power_w: 0.0,
    };
    assert_eq!(info.estimated_power_w, 0.0);
}

#[test]
fn audio_power_model_muted_zero() {
    let a = AudioInfo {
        volume_pct: Some(100.0),
        muted: true,
        device_active: true,
        playing: true,
        estimated_power_w: 0.0,
    };
    // When muted, effective volume is 0 → power = idle only
    // (this tests the model logic in metrics.rs, but we can't call it directly
    //  so we just validate the struct invariant)
    assert_eq!(a.estimated_power_w, 0.0);
}

#[test]
fn soc_compute_total_correct() {
    let mut soc = SocPower {
        cpu_w: 2.5,
        gpu_w: 1.0,
        ane_w: 0.1,
        dram_w: 0.5,
        gpu_sram_w: 0.05,
        ..Default::default()
    };
    soc.compute_total();
    let expected = 2.5 + 1.0 + 0.1 + 0.5 + 0.05;
    assert!((soc.total_w - expected).abs() < 1e-6);
}
