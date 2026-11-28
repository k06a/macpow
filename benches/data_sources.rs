//! Latency of one read of each live data source macpow polls, on the hardware it runs on.
//! For a quick table:
//! `cargo bench --bench data_sources -- --warm-up-time 1 --measurement-time 2`
//!
//! A source this machine does not have is skipped with a message. Timing its failed lookup
//! would put a meaninglessly small number under the source's name.

use criterion::{criterion_group, criterion_main, Criterion};
use macpow::ioreport::IOReportSampler;
use macpow::smc::SmcConnection;
use macpow::{battery, metrics, peripherals};

/// Benchmarks `read` under `name` if its first result passes `available`, else reports a skip.
fn bench_source<T>(
    c: &mut Criterion,
    name: &str,
    mut read: impl FnMut() -> T,
    available: impl FnOnce(&T) -> bool,
) {
    if available(&read()) {
        c.bench_function(name, |b| b.iter(&mut read));
    } else {
        eprintln!("skipping {name}: not available on this machine");
    }
}

fn bench_ioreport(c: &mut Criterion) {
    let sampler = match IOReportSampler::new() {
        Ok(sampler) => sampler,
        Err(e) => {
            eprintln!("skipping ioreport_sample: {e:#}");
            return;
        }
    };
    bench_source(c, "ioreport_sample", || sampler.sample(), |s| s.is_ok());
}

fn bench_smc(c: &mut Criterion) {
    let mut smc = match SmcConnection::open() {
        Ok(smc) => smc,
        Err(e) => {
            eprintln!("skipping smc_temperatures, smc_fans, smc_system_power: {e:#}");
            return;
        }
    };

    // read_temperatures() returns nothing until key discovery has finished, which the sampler
    // thread arranges at start-up. Without it this would time an early return (3 ns) instead
    // of the sweep over every sensor, the most expensive read macpow does per tick.
    let discovery = smc.start_temp_discovery();
    smc.finish_temp_discovery(discovery);
    let sensors = smc.read_temperatures().len();
    eprintln!("smc_temperatures: one sweep reads {sensors} sensors");

    bench_source(
        c,
        "smc_temperatures",
        || smc.read_temperatures(),
        |t| !t.is_empty(),
    );
    bench_source(c, "smc_fans", || smc.read_fans(), |f| !f.is_empty());
    bench_source(
        c,
        "smc_system_power",
        || smc.read_system_power(),
        |w| *w > 0.0,
    );
}

fn bench_battery(c: &mut Criterion) {
    bench_source(c, "battery", battery::read_battery, |b| b.present);
}

fn bench_peripherals(c: &mut Criterion) {
    bench_source(c, "wifi", peripherals::read_wifi_info, |w| {
        !w.interface_name.is_empty()
    });
    // These enumerate whatever is attached right now: an empty list is a valid result, and the
    // cost grows with the number of entries.
    c.bench_function("bluetooth", |b| b.iter(peripherals::read_bluetooth_devices));
    c.bench_function("usb_devices", |b| b.iter(peripherals::list_usb_devices));
    c.bench_function("power_assertions", |b| {
        b.iter(peripherals::list_power_assertions)
    });
}

fn bench_metrics(c: &mut Criterion) {
    bench_source(
        c,
        "display_brightness",
        metrics::read_display_brightness,
        Option::is_some,
    );
    bench_source(
        c,
        "audio_volume",
        metrics::read_audio_volume,
        |(volume, _)| volume.is_some(),
    );
    c.bench_function("cpu_ticks", |b| b.iter(metrics::read_cpu_ticks));
    c.bench_function("memory", |b| b.iter(metrics::read_mem_used_gb));
    c.bench_function("gpu_utilization", |b| b.iter(metrics::read_gpu_utilization));
    c.bench_function("process_energy", |b| {
        b.iter(metrics::read_all_process_energy)
    });
}

criterion_group!(
    benches,
    bench_ioreport,
    bench_smc,
    bench_battery,
    bench_peripherals,
    bench_metrics,
);
criterion_main!(benches);
