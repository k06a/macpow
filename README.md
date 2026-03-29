# macpow

Real-time power consumption monitor for Apple Silicon Macs (M1–M5).

macpow reads directly from macOS hardware interfaces — IOReport, SMC, IORegistry, and kernel APIs — to show per-component power draw, temperatures, frequencies, and per-process energy attribution. No sudo required.

## Features

- **SoC breakdown** — CPU (E/P cores), GPU, ANE, DRAM, GPU SRAM with real-time wattage from IOReport Energy Model
- **Real frequencies** — CPU and GPU MHz from DVFS voltage-states tables, not percentages
- **Temperatures** — per-component (CPU, GPU, DRAM, SSD, Battery) from SMC sensors with min/max tracking
- **Display & keyboard** — live brightness and estimated power via DisplayServices and IORegistry PWM
- **Battery** — voltage, amperage, charge %, time remaining, drain/charge rate
- **SSD** — NVMe power estimation based on disk I/O throughput
- **Peripherals** — WiFi signal/mode, Bluetooth devices with battery levels, USB device tree
- **Per-process energy** — top 10 processes by energy consumed (from `proc_pid_rusage`)
- **Fans** — RPM and cubic power model
- **Collapsible tree** — fold/unfold sections with arrow keys
- **Sparkline charts** — pin any resource with Space to track its power history
- **Time-based SMA** — toggle 0s/5s/10s smoothing window
- **JSON mode** — pipe structured data for scripts and dashboards
- **No sudo** — runs entirely with user-level permissions

## Install

### From source

```bash
git clone https://github.com/k06a/macpow.git
cd macpow
cargo build --release
./target/release/macpow
```

### With cargo

```bash
cargo install --git https://github.com/k06a/macpow.git
```

## Usage

```
macpow                    # TUI mode (default)
macpow --json             # JSON output to stdout
macpow --interval 1000    # Set sampling interval in ms (default: 500)
```

### Keybindings

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `Up` / `Down` / `j` / `k` | Move cursor |
| `Left` / `Right` / `h` / `l` | Collapse / expand tree node |
| `Space` | Pin/unpin resource chart |
| `a` | Cycle SMA window: 0s / 5s / 10s |
| `r` | Reset all totals and min/max |
| `PgUp` / `PgDn` | Scroll by 10 rows |
| `Home` | Jump to top |

All letter keys work on any keyboard layout (QWERTY, Russian, Dvorak, etc).

## Architecture

```
┌─────────────┬──────────────────────────────┐
│ Data source │ What it provides             │
├─────────────┼──────────────────────────────┤
│ IOReport    │ SoC power (Energy Model),    │
│             │ CPU/GPU frequencies (DVFS)   │
│ SMC         │ System power (PSTR), temps,  │
│             │ fans, keyboard backlight     │
│ IORegistry  │ Battery, display brightness, │
│             │ keyboard PWM, USB devices    │
│ proc_pid_   │ Per-process billed energy    │
│ rusage      │                              │
│ system_     │ WiFi, Bluetooth devices      │
│ profiler    │                              │
│ netstat     │ Network traffic counters     │
│ iostat      │ Disk I/O throughput          │
└─────────────┴──────────────────────────────┘
```

Each data source runs in its own thread, updating shared metrics independently. The TUI renders at the configured interval without blocking on slow sources.

### Power measurements vs estimates

| Component | Source | Method |
|-----------|--------|--------|
| CPU, GPU, ANE, DRAM | IOReport | Direct energy measurement (mJ/uJ/nJ deltas) |
| System total | SMC PSTR | Direct power rail measurement |
| Battery | IORegistry | V * I calculation |
| Per-process | Kernel | `ri_billed_energy` from rusage_info_v4 |
| Display | DisplayServices | Brightness * 5W max (linear model) |
| Keyboard | IORegistry PWM | Duty cycle * 0.5W max |
| Fans | SMC RPM | Cubic model: (RPM/RPM_max)^3 * 1W |
| Audio | osascript + pmset | Idle 0.05W + volume^2 * 1W |
| WiFi | system_profiler | RSSI-based model: 0.1–0.8W |
| Bluetooth | system_profiler | Fixed per device type (0.01–0.05W) |
| SSD | iostat | I/O utilization: 0.03–2.5W |

## Requirements

- macOS 12+ (Monterey or later)
- Apple Silicon (M1, M2, M3, M4, M5 — any variant)
- Rust 1.70+

## License

[MIT](LICENSE)
