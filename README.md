# macpow

Real-time power consumption monitor for Apple Silicon Macs (M1–M5).

macpow reads directly from macOS hardware interfaces — IOReport, SMC, IORegistry, and kernel APIs — to show per-component power draw, temperatures, frequencies, CPU utilization, and per-process energy attribution. No sudo required.

## Features

- **SoC breakdown** — CPU (E/P cores with per-core power, utilization bars, temperatures), GPU, ANE, DRAM, GPU SRAM with real-time wattage from IOReport Energy Model
- **CPU utilization** — per-core usage % with visual bars from Mach `host_processor_info`
- **Real frequencies** — CPU and GPU MHz from DVFS voltage-states tables, not percentages
- **Temperatures** — per-component and per-core from SMC sensors (CPU, GPU, ANE, DRAM, SSD, Battery)
- **Memory** — used/total GB for DRAM
- **Display & keyboard** — live brightness (nits) and estimated power via DisplayServices and IORegistry PWM
- **Battery** — voltage, amperage, charge %, time remaining, temperature, drain/charge rate
- **SSD** — model, interconnect (Apple Fabric/PCIe), power estimation, read/write counters
- **Peripherals** — WiFi (signal/mode/channel), Bluetooth devices with battery levels, USB devices (speed/power/I/O counters)
- **Per-process energy** — dynamically-sized top processes by session energy (from `proc_pid_rusage`), dead process detection
- **Fans** — RPM and cubic power model per fan
- **Collapsible tree** — fold/unfold with arrows, `+`/`-` for all, fully expanded at start
- **Sparkline charts** — pin any resource with Space, inline 1-line history column at wide terminals
- **Time-based SMA** — toggle 0s/5s/10s smoothing window
- **Latency control** — toggle UI refresh rate: 500ms / 2s / 5s
- **Mouse support** — click to select rows
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

```
+---------------+--------------------------------+
| Data source   | What it provides               |
+---------------+--------------------------------+
| IOReport      | SoC power (Energy Model),      |
|               | CPU/GPU frequencies (DVFS)     |
| SMC           | System power (PSTR), temps,    |
|               | fans, keyboard backlight       |
| IORegistry    | Battery, display brightness,   |
|               | keyboard PWM, USB devices,     |
|               | SSD model, disk I/O stats      |
| Mach API      | Per-CPU utilization ticks       |
| proc_pid_     | Per-process billed energy      |
| rusage        |                                |
| system_       | WiFi, Bluetooth devices        |
| profiler      |                                |
| netstat       | Network traffic counters       |
| iostat        | Disk I/O throughput            |
| vm_stat       | Memory usage                   |
+---------------+--------------------------------+
```

Each data source runs in its own thread, updating shared metrics independently. The TUI renders at the configured interval without blocking on slow sources.

### Power measurements vs estimates

| Component | Source | Method |
|-----------|--------|--------|
| CPU, GPU, ANE, DRAM | IOReport | Direct energy measurement (mJ/uJ/nJ deltas) |
| System total | SMC PSTR | Direct power rail measurement |
| Battery | IORegistry | V * I calculation |
| Per-process | Kernel | `ri_billed_energy` from rusage_info_v4 |
| CPU utilization | Mach API | `host_processor_info` tick deltas |
| Display | DisplayServices | Brightness * 5W max (linear model) |
| Keyboard | IORegistry PWM | Duty cycle * 0.5W max |
| Fans | SMC RPM | Cubic model: (RPM/RPM_max)^3 * 1W |
| Audio | osascript + pmset | Idle 0.05W + volume^2 * 1W |
| WiFi | system_profiler | RSSI-based model: 0.1-0.8W |
| Bluetooth | system_profiler | Fixed per device type (0.01-0.05W) |
| SSD | iostat | I/O utilization: 0.03-2.5W |
| USB | IORegistry | bMaxPower * 5V |

## Requirements

- macOS 12+ (Monterey or later)
- Apple Silicon (M1, M2, M3, M4, M5 -- any variant)
- Rust 1.70+

## License

[MIT](LICENSE)
