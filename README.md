# macpow

[![CI](https://github.com/k06a/macpow/actions/workflows/ci.yml/badge.svg)](https://github.com/k06a/macpow/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/macpow)](https://crates.io/crates/macpow)
[![Homebrew](https://img.shields.io/badge/homebrew-v0.1.5-orange?logo=homebrew)](https://github.com/k06a/homebrew-tap)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Apple%20Silicon-black?logo=apple)](https://github.com/k06a/macpow)

Real-time power consumption monitor for Apple Silicon Macs (M1–M5+).

<p align="center">
  <img src="./screenshot.png" width="75%" alt="macpow screenshot">
</p>

**macpow** reads directly from macOS hardware interfaces — IOReport, SMC, IORegistry, CoreAudio, and Mach/kernel APIs — to show per-component power draw, temperatures, frequencies, CPU utilization, and per-process energy attribution. No sudo required.

## Features

- **SoC breakdown** — CPU (E/P cores with per-core power, utilization bars, temperatures), GPU, ANE, DRAM, GPU SRAM, Media Engine, Camera (ISP), Fabric — all from IOReport Energy Model
- **CPU utilization** — per-core usage % with visual bars from Mach `host_processor_info`
- **Real frequencies** — CPU and GPU MHz from DVFS voltage-states tables, not percentages
- **Temperatures** — per-component and per-core from SMC sensors (CPU, GPU, ANE, DRAM, SSD, Battery); adaptive key mapping for M1–M3 (`Tp0*`) and M4+ (`Tex*`/`Tp1*`/`Tp2*`)
- **Memory** — used/total GB via `host_statistics64` Mach API
- **Display** — brightness estimate + IOReport SoC display controller; external display power via IOReport DISPEXT
- **Keyboard** — backlight brightness and estimated power via IORegistry PWM
- **Battery** — voltage, amperage, charge %, time remaining, temperature, drain/charge rate
- **SSD** — model, interconnect (Apple Fabric/PCIe), power estimation from IORegistry disk counters
- **Peripherals** — Thunderbolt/PCIe (IOReport measured), WiFi (signal/mode/channel), Bluetooth devices with battery levels, USB devices (speed/power/I/O counters)
- **Per-process energy** — dynamically-sized top processes by session energy (from `proc_pid_rusage`), dead process detection
- **Fans** — RPM and cubic power model per fan
- **Collapsible tree** — fold/unfold with arrows, `+`/`-` for all
- **Sparkline charts** — pin any resource with Space, inline 1-line history column at wide terminals
- **Time-based SMA** — toggle 0s/5s/10s smoothing window
- **Latency control** — toggle UI refresh rate: 500ms / 2s / 5s
- **Mouse support** — click to select rows
- **JSON mode** — pipe structured data for scripts and dashboards
- **No sudo** — runs entirely with user-level permissions

## Install

### With cargo

```bash
cargo install macpow
```

### From source

```bash
git clone https://github.com/k06a/macpow.git
cd macpow
cargo build --release
./target/release/macpow
```

### Homebrew

```bash
brew tap k06a/tap
brew install macpow
```

## Usage

```
macpow                    # TUI mode (default)
macpow --json             # JSON output to stdout
macpow --interval 1000    # Set sampling interval in ms (default: 500)
macpow --dump             # Dump IOReport channel names (diagnostics)
```

### Keybindings

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `Up` / `Down` / `j` / `k` | Move cursor |
| `Left` / `Right` / `h` | Collapse / expand tree node |
| `+` / `=` | Expand all nodes |
| `-` | Collapse all nodes |
| `Space` | Pin/unpin resource chart |
| `a` | Cycle SMA window: 0s / 5s / 10s |
| `l` | Cycle UI latency: 500ms / 2s / 5s |
| `r` | Reset all totals and min/max |
| `PgUp` / `PgDn` | Scroll by 10 rows |
| `Home` | Jump to top |
| Mouse click | Select row |

All letter keys work on any keyboard layout (QWERTY, Russian, Dvorak, etc).

## Architecture

Each data source runs in its own thread, updating shared metrics at its own pace. The TUI renders at the configured interval without blocking on slow sources.

```
+------------------+---------------------------------------------+
| Data source      | What it provides                            |
+------------------+---------------------------------------------+
| IOReport         | SoC power (Energy Model),                   |
|                  | CPU/GPU frequencies (DVFS residency)        |
| SMC              | System power (PSTR), temps, fans            |
| IORegistry       | Battery, display brightness, keyboard PWM,  |
|                  | USB devices, SSD model, disk I/O counters   |
| CoreAudio        | Volume level, mute state                    |
| Mach API         | Per-CPU utilization ticks, memory stats      |
| proc_pid_rusage  | Per-process billed energy                   |
| getifaddrs       | Network traffic byte counters               |
| system_profiler  | WiFi info, Bluetooth devices                |
| IOPMAssertions   | Power assertions, audio playback detection  |
+------------------+---------------------------------------------+
```

### Power measurements vs estimates

| Component | Source | Method |
|-----------|--------|--------|
| CPU, GPU, ANE, DRAM | IOReport | Direct energy measurement (mJ/uJ/nJ deltas) |
| Media Engine, Camera (ISP) | IOReport | Direct energy measurement (AVE + MSR, ISP) |
| Fabric (AMCC, DCS, FAB, AFR) | IOReport | Direct energy measurement |
| Thunderbolt/PCIe | IOReport | Direct energy measurement (PCIe ports + controllers) |
| Display | DisplayServices + IOReport | Brightness * 5W + SoC controller (DISP) + external (DISPEXT) |
| System total | SMC PSTR | Direct power rail measurement |
| Battery | IORegistry | V * I calculation |
| Per-process | Kernel | `ri_billed_energy` from rusage_info_v4 |
| CPU utilization | Mach API | `host_processor_info` tick deltas |
| Memory | Mach API | `host_statistics64` (active + inactive + wired + compressor pages) |
| Keyboard | IORegistry PWM | Duty cycle * 0.5W max |
| Fans | SMC RPM | Cubic model: (RPM/RPM_max)^3 * 1W |
| Audio | CoreAudio + IOPMAssertions | Idle 0.05W + volume^2 * 1W |
| WiFi | system_profiler | RSSI-based model: 0.1-0.8W |
| Bluetooth | system_profiler | Fixed per device type (0.01-0.05W) |
| SSD | IORegistry counters | I/O utilization: 0.03-2.5W |
| Network | getifaddrs | Byte counters (no power model, data only) |
| USB | IORegistry | bMaxPower * 5V |

## Requirements

- macOS 12+ (Monterey or later)
- Apple Silicon (M1, M2, M3, M4, M5 — any variant)
- Rust 1.70+

## License

[MIT](LICENSE)
