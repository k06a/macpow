# 💻🔋 macpow – Real-time power tree TUI for Apple Silicon

[![CI](https://github.com/k06a/macpow/actions/workflows/ci.yml/badge.svg)](https://github.com/k06a/macpow/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/macpow)](https://crates.io/crates/macpow)
[![Homebrew](https://img.shields.io/badge/homebrew-v0.1.20-orange?logo=homebrew)](https://github.com/k06a/homebrew-tap)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Apple%20Silicon-black?logo=apple)](https://github.com/k06a/macpow)

Real-time power consumption monitor for Apple Silicon Macs (M1–M5+).

<p align="center">
  <img src="./screenshot.png" width="75%" alt="macpow screenshot">
</p>

**macpow** reads directly from macOS hardware interfaces — IOReport, SMC, IORegistry, CoreAudio, and Mach/kernel APIs — to show per-component power draw, temperatures, frequencies, CPU utilization, and per-process energy attribution. No sudo required.

### Legend

| Symbol | Meaning |
|--------|---------|
| `0.123 W` | Measured power (direct hardware reading) |
| `≈0.123 W` | Estimated power (model-based calculation) |
| `≤0.123 W` | Upper-bound power estimate |
| `▸` | Pinned resource (sparkline chart visible) |
| `▓▓▓░░░░░░░` | CPU core utilization bar (filled = busy) |
| `37°C` | Fresh temperature reading |
| `~37°C` | Stale temperature (sensor read failed, showing last known value) |
| `pending…` | Data source still initializing |
| `[dead]` | Process has exited (energy total preserved) |
| **Bold white** | Section headers and measured values |
| <span style="color:green">Green</span> | Low power (< 1W) or info-only rows |
| <span style="color:goldenrod">Yellow</span> | Moderate power (1–5W) |
| <span style="color:orange">Orange</span> | High power (5–10W) |
| <span style="color:red">Red</span> | Very high power (> 10W) |
| Gray | Dimmed/inactive items |

## Features

- **SoC breakdown** — CPU (core tiers with per-core power, utilization bars, temperatures), GPU, ANE, DRAM, GPU SRAM, Media Engine, Camera (ISP), Fabric — from the IOReport Energy Model, or from CLPC energy counters on macOS 27+ where the Energy Model is no longer live (see [macOS 27](#macos-27-energy-model-is-gated))
- **CPU utilization** — per-core usage % with visual bars from Mach `host_processor_info`
- **Real frequencies** — CPU and GPU MHz from DVFS voltage-states tables, not percentages
- **Temperatures** — per-component and per-core from SMC sensors (CPU, GPU, ANE, DRAM, SSD, Battery); universal bank-based key mapping for all Apple Silicon generations (M1–M5+, including Ultra dual-die); stale value caching with `~` indicator when sensors temporarily read invalid
- **Memory** — used/total GB via `host_statistics64` Mach API
- **Display** — brightness, panel class (SDR/HDR/XDR), refresh rate / ProMotion, live HDR-active indicator from `AppleARMBacklight` IOReport `DPB factor`, plus IOReport SoC display controller and external display power via DISPEXT
- **Keyboard** — backlight brightness and estimated power via IORegistry PWM
- **Battery** — voltage, amperage, charge %, time remaining, temperature, drain/charge rate
- **SSD** — model, interconnect (Apple Fabric/PCIe), power estimation from IORegistry disk counters
- **Peripherals** — Thunderbolt/PCIe (IOReport measured), Ethernet (link speed, per-interface traffic), WiFi (signal/mode/channel, per-interface traffic), Bluetooth devices with battery levels, USB devices (speed/power/I/O counters)
- **Per-process energy** — dynamically-sized top processes by session energy (from `proc_pid_rusage`), with per-process disk I/O rates, network traffic (via nettop), RAM footprint, dead process detection
- **Fans** — RPM and cubic power model per fan
- **Collapsible tree** — fold/unfold with arrows, `+`/`-` for all
- **Sparkline charts** — pin any resource with Space, inline 1-line history column at wide terminals
- **Time-based SMA** — toggle 0s/5s/10s smoothing window
- **Latency control** — toggle UI refresh rate: 1x / 2x / 4x / 8x of `--interval` (`l`)
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
cargo build --release      # rustup first installs the Rust release pinned in rust-toolchain.toml
./target/release/macpow
```

### Homebrew

```bash
brew tap k06a/tap
brew install macpow
```

### With pixi (conda-forge)

```bash
pixi global install macpow
# execute without installing
pixi exec macpow
```

## Usage

```
macpow                    # TUI mode (default)
macpow --json             # JSON Lines on stdout: one compact object per line
macpow --json | jq .      # the same stream, pretty-printed
macpow --interval 500     # Sampling interval in ms, 100 to 60000 (default: 250)
macpow --dump             # Dump IOReport channel names (diagnostics)
macpow --dump-smc         # Dump every SMC key with type and value (diagnostics)
```

`--interval` is the base sampling period of every source. Three sources that each start a subprocess refresh no more often than their own period: Bluetooth batteries every 10 s, per-process network counters every 2 s and the Wi-Fi SSID fallback every 30 s. `--interval` is also the TUI's initial refresh interval (shown in the footer, changed with `l`) and the period of the `--json` stream.

`--json` writes one object per `--interval`. Each object is compact JSON on a single line ([JSON Lines](https://jsonlines.org)), flushed as soon as it is produced, so `while read`, `grep` and `jq -c` can consume the stream record by record. When the reader closes the pipe (`macpow --json | head -1`) macpow exits with status 0; Ctrl+C ends it by SIGINT like any other filter.

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
| `l` | Cycle refresh interval: 1x / 2x / 4x / 8x of `--interval` (250ms / 500ms / 1s / 2s by default) |
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
| IOReport         | SoC power (Energy Model; on macOS 27+ CLPC  |
|                  | energy counters, PMP histograms as backup), |
|                  | CPU/GPU frequencies (DVFS residency)        |
| SMC              | System power (PSTR), display backlight      |
|                  | (PBwo on M5/Neo, PDBR on M1-M4),            |
|                  | adapter (PDTR), WiFi (wiPm), temps, fans    |
| IORegistry       | Battery, display brightness, keyboard PWM,  |
|                  | USB devices, SSD model, disk I/O counters   |
| CoreAudio        | Volume level, mute state                    |
| Mach API         | Per-CPU utilization ticks, memory stats     |
| proc_pid_rusage  | Per-process CPU energy                      |
| getifaddrs       | Network traffic byte counters               |
| CoreWLAN/pmset   | WiFi info, Bluetooth devices                |
| IOPMAssertions   | Power assertions, audio playback detection  |
+------------------+---------------------------------------------+
```

### Power measurements vs estimates

| Component | Source | Method |
|-----------|--------|--------|
| CPU, GPU, ANE, DRAM | IOReport | Direct energy measurement (mJ/uJ/nJ deltas) |
| CPU cores on macOS 27+ | IOReport CPU Stats | Estimate (`≈`): cores' share of cluster power split by V²·f-weighted residency |
| Media Engine, Camera (ISP) | IOReport | Direct energy measurement (AVE + MSR, ISP) |
| Fabric (AMCC, DCS, FAB, AFR) | IOReport | Direct energy measurement |
| Thunderbolt/PCIe | IOReport | Direct energy measurement (PCIe ports + controllers) |
| Display backlight | SMC PBwo (M5 Pro/Max/Neo, A18) / PDBR (M1-M4 XDR) | Direct power rail measurement |
| Display controller | IOReport DISP/DISPEXT | Direct energy measurement (SoC + external) |
| Power adapter | SMC PDTR | Direct power delivery measurement |
| System total | SMC PSTR | Direct power rail measurement |
| Battery | IORegistry | V * I calculation |
| Per-process | Kernel | `ri_energy_nj` (CPU energy) from rusage_info_v6, or `ri_billed_energy` from rusage_info_v4 on kernels without v6; without root only the current user's processes are readable |
| Per-process disk I/O | Kernel | `ri_diskio_bytesread/written` from rusage_info_v4 |
| Per-process memory | Kernel | `ri_phys_footprint` from rusage_info_v4 |
| Per-process network | nettop | Cumulative bytes per process (~18ms subprocess) |
| CPU utilization | Mach API | `host_processor_info` tick deltas |
| Memory | Mach API | `host_statistics64` (active + inactive + wired + compressor pages) |
| Keyboard | IORegistry PWM | Duty cycle * 0.5W max |
| Fans | SMC RPM | Cubic model: (RPM/RPM_max)^3 * 1W |
| Audio | CoreAudio + IOPMAssertions | Idle 0.05W + volume^2 * 1W |
| WiFi | SMC wiPm | Direct power measurement |
| Bluetooth | pmset | Fixed per device type (0.01-0.05W) |
| SSD | IORegistry counters | I/O utilization: 0.03-2.5W |
| Ethernet | getifaddrs | Link detection, speed, per-interface traffic (data only) |
| Network | getifaddrs | Per-interface byte counters for Ethernet and WiFi |
| USB | IORegistry PowerOutDetails | Per-port power measurement (Watts/PDPowermW) |

### macOS 27: Energy Model is gated

Since macOS 27 the kernel recomputes the IOReport `Energy Model` counters (CPU, per-core, DRAM, ANE, …) only when the sampling process holds the private entitlement `com.apple.private.pmgr.nrg.reporting`. Everyone else — root included — reads values left behind by the last entitled sampler (`powermetrics`, the powerlog daemons): frozen for minutes, then one catch-up jump worth kilowatts if divided by a sample interval. This affects every Apple Silicon Mac, not a particular chip. Only `GPU Energy` (published by the GPU driver) stays live.

macpow detects this at runtime and switches sources:

| Figure | Source on macOS 27+ | Quality |
|--------|---------------------|---------|
| CPU total, ANE, DRAM | AppleCLPC energy counters (same accumulators as the Energy Model, not gated) | Exact |
| GPU | `GPU Energy` from the GPU driver, as before (it was never gated) | Exact |
| Cluster totals | CLPC per-cluster counter where its ID is known (M5 Pro/Max); otherwise the CPU total divided in proportion to the PMP power histograms (M5 family) or to DVFS residency | Exact / estimate (`≈`) |
| Per-core power | Cluster power × core share × each core's V²·f-weighted active residency. The core share (≈ 0.6–0.75) is the part of a cluster's energy the Energy Model books to cores rather than to the cluster's shared logic and SRAM, so cores add up to less than their cluster, as they do with a live Energy Model | Estimate (`≈`) |
| Fabric | DCS counter + "CLPC package total minus everything measured separately" — shown as `Fabric + other SoC` because it also holds GPU SRAM, ISP, media and display-controller power | Exact sum |
| SoC total | Sum of the rows above, i.e. the CLPC package total with the GPU taken from its own counter | Exact |

Checked on an M5 Max (macOS 27.0.1) against the exact energies that an entitled sampler publishes, over 5–9 minute windows: CPU total, cluster totals, DRAM and fabric within 0.1–1.4 %; per-core power within ±5 % for most loaded cores, with one core of the Super cluster consistently 10–14 % high.

CLPC reports are not listed in the IOReport legend; they are addressed by opaque IDs that differ per SoC driver and per macOS major version (`src/clpc.rs`; the scalar IDs come from the [macmon](https://github.com/vladkens/macmon) project). At startup macpow checks which of them the driver really delivers. Where the per-cluster counter is missing it subscribes to the PMP histograms instead; where no CLPC counter works at all (an unknown driver, or a macOS version whose IDs aren't in the table) the histograms also supply an estimated CPU total and the remaining rails are reported as unavailable (`rails_unavailable` in `--json`, `n/a` in the tree) rather than as 0 W. Macs without PMP histograms (M1–M4) then have no CPU power source at all (`cpu_unavailable`).

`--json` keeps its field names and adds flags that are present only when true: `energy_model_gated`, `cores_estimated`, `clusters_estimated`, `cpu_estimated`, `cpu_unavailable`, `rails_merged`, `rails_unavailable`.

### CPU tiers

The tree labels the two CPU tiers by the names macOS reports in `hw.perflevelN.name`: `E-Cores` / `P-Cores` on M1–M4 and base M5, `P-Cores` / `S-Cores` (Performance / Super) on M5 Pro/Max. `--json` keeps `ecpu_clusters` (lower tier) and `pcpu_cluster` (top tier) and adds `cpu_tier_names`.

Logical CPU numbering is chip-specific — M1–M4 list the efficiency cluster first, M5 Pro/Max list the Super cluster last — so per-core utilization is matched to cores through the device-tree topology (`cpu_high_tier` in `--json`), not by position.

### IOReport channel naming (multi-die support)

IOReport channel names vary between single-die and multi-die (Ultra) chips. The parser handles both generically:

```
Single-die (M1/M2/M3/M4 base/Pro/Max):
  CPU Stats:    ECPU0, PCPU10             ← digit suffix
  Energy Model: EACC_CPU0, PACC0_CPU5     ← _CPU suffix
  Blocks:       ISP, DRAM, ANE, DISP      ← bare names

M5 Pro/Max (Performance = MCPU clusters, Super = PCPU):
  CPU Stats:    MCPU00, MCPU15, PCPU3     ← MCPU{cluster}{core}
  Energy Model: MCPU1_2, PACC_3           ← {cluster}_{core}
  Clusters:     MCPU0, MCPU1, PCPU

Multi-die (M1/M2/M3 Ultra):
  CPU Stats:    DIE_0_ECPU_CPU0, DIE_1_PCPU1_CPU3   ← DIE_N_ prefix + _CPU suffix
  Energy Model: DIE_0_EACC_CPU0, DIE_1_PACC1_CPU3   ← DIE_N_ prefix + _CPU suffix
  Blocks:       ISP0_0, DRAM0_1, ANE0_0             ← per-die suffix
```

Two design rules keep this forward-compatible with future chips:

1. **`strip_die_prefix`** generically removes `DIE_{N}_` so downstream parsers always see the same base format
2. **`starts_with`** matching for block power handles any suffix Apple may add (e.g. `ISP` matches `ISP`, `ISP0_0`, `ISP0_1`, etc.)

If a new chip isn't detected correctly, run `macpow --dump` to see the raw IOReport channel names.

## Requirements

- macOS 12+ (Monterey or later)
- Apple Silicon (M1, M2, M3, M4, M5 — any variant)
- Rust 1.88+ for `cargo install` or building from source (declared as `rust-version` in `Cargo.toml`)

## Release checklist

> **Toolchain pinning.** `rust-toolchain.toml` pins an exact Rust release.
> rustup installs it on first use, locally and in CI, so local clippy/fmt
> always see the same lints CI does and there is nothing to update before a
> release. A newer Rust, and the lints it adds, arrives only through a
> commit that changes the version in that file — make it a commit of its
> own, so CI tests it like any other change.
>
> **Dependency age.** No dependency is used before it is 7 days old: not a
> Rust release, a crate or a GitHub Action. `.cargo/config.toml` makes Cargo
> skip younger crate versions when it resolves, Dependabot waits 7 days
> before proposing an update, and CI fails if `Cargo.lock` pins a younger
> version (`python3 -I .github/scripts/lock-age.py`).

```bash
# 1. Bump version
vim Cargo.toml                        # update version = "X.Y.Z"
cargo check                           # writes the new version into Cargo.lock (CI builds with --locked)

# 2. Build, lint, test — must all pass before the version bump commit
cargo fmt --check                     # formatting clean
# Clippy: the same command CI runs. The lints the project allows are listed in
# Cargo.toml under [lints.clippy], so no -A flags are needed.
cargo clippy --all-targets --locked -- -D warnings
cargo test                            # all tests pass
cargo build --release --locked        # final binary build

# 3. Update Homebrew badge in README.md
#    Change: homebrew-vX.Y.Z in the badge URL

# 4. Commit, then dry-run the publish so any cargo-side issues show up before tagging
#    -a stages tracked files only (Cargo.toml, Cargo.lock, README.md). Don't use
#    `git add -A`: it would also commit drafts and dumps lying in the directory.
git commit -am "Bump version to X.Y.Z"
#    cargo refuses to package uncommitted changes, which is why this comes after the
#    commit. Don't pass --allow-dirty; if the dry run fails, fix and `git commit -a --amend`.
cargo publish --dry-run

# 5. Tag, push
git tag vX.Y.Z
git push origin main --tags
# The tag run builds the binary on a clean runner and creates the GitHub Release with
# macpow-aarch64-apple-darwin.tar.gz, its .sha256 and a build provenance attestation.
# It fails if the tag is not "v" + the Cargo.toml version, or if a release for the
# tag already exists.

# 6. Publish to crates.io  (immutable — only after CI is green!)
cargo publish

# 7. Update Homebrew tap (via PR to trigger bottle building)
curl -sL https://github.com/k06a/macpow/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256
# Update url + sha256 in homebrew-tap/Formula/macpow.rb
cd ../homebrew-tap
git checkout main && git pull
git checkout -b update-macpow-X.Y.Z
# edit Formula/macpow.rb (url + sha256 only — bottle block is auto-rewritten)
git commit -am "Update macpow to X.Y.Z"
git push origin update-macpow-X.Y.Z
# Create PR, wait for CI to build bottles, then add label "pr-pull"
# publish.yml will upload bottles and merge the PR

# 8. Update conda-forge feedstock
cd ../macpow-feedstock
git checkout main && git pull origin main
git checkout -b bump-X.Y.Z
# edit recipe/recipe.yaml: bump `version:` and update `sha256:` (same value as step 7)
git commit -am "macpow vX.Y.Z"
git push fork bump-X.Y.Z   # fork remote, since origin is conda-forge upstream
# Open PR against conda-forge/macpow-feedstock from your fork branch
```

If CI fails AFTER you ran `cargo publish` (step 6), that crates.io version
is immutable. Yank it (`cargo yank --version X.Y.Z`) and bump the patch
number — don't try to force-push the tag.

## Contributing

Before submitting a PR, please run:

```bash
cargo fmt --check          # code formatting
cargo clippy --all-targets -- -D warnings   # the same lints CI enforces
cargo test                 # unit + integration tests
cargo build --release      # final build check
```

All four must pass with zero errors. The lints the project intentionally allows (FFI / Objective-C style) are listed under `[lints.clippy]` in `Cargo.toml`, so plain `cargo clippy`, your editor and CI all apply the same rules. `rust-toolchain.toml` pins the Rust release they all use; rustup installs it on first use.

### Collecting diagnostics

If per-core temperatures are missing or incorrect on your Mac, please open an issue with:

```bash
macpow --dump > dump.txt                       # IOReport channel names
macpow --dump-smc > smc.txt                    # all SMC keys (incl. power rails)
macpow --json > metrics.json                   # full metrics (Ctrl+C after ~15s)
system_profiler SPHardwareDataType | head -10  # chip model
```

This helps add support for new Apple Silicon variants.

## License

[MIT](LICENSE)
