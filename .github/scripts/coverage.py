#!/usr/bin/env python3
"""Show which data macpow really obtained on one or more machines.

Each argument is a directory written by the "probe" job of
.github/workflows/macos-matrix.yml:

    identity.txt          key=value lines describing the machine and exit codes
    metrics.json          the JSON objects printed by `macpow --json`
    hardware-tests.txt    output of `cargo test --test integration`
    ioreport-channels.txt output of `macpow --dump`
    smc-keys.txt          output of `macpow --dump-smc`

Prints Markdown tables with one column per directory, so the same script
serves a single machine and the comparison across the whole matrix.
"""

import json
import re
import sys
from pathlib import Path

# (label, kind, JSON path, unit). Kinds:
#   num   - highest number seen in any sample
#   count - longest list seen in any sample
#   flag  - true in any sample
#   text  - last non-empty string
ROWS = [
    ("CPU power", "num", "soc.cpu_w", "W"),
    ("CPU clusters (low tier)", "count", "soc.ecpu_clusters", ""),
    ("CPU cores (high tier)", "count", "soc.pcpu_cluster.cores", ""),
    ("CPU frequency", "num", "soc.pcpu_freq_mhz", "MHz"),
    ("GPU power", "num", "soc.gpu_w", "W"),
    ("GPU frequency", "num", "soc.gpu_freq_mhz", "MHz"),
    ("GPU utilisation", "num", "soc.gpu_util_device", "%"),
    ("ANE power", "num", "soc.ane_w", "W"),
    ("DRAM power", "num", "soc.dram_w", "W"),
    ("SoC total power", "num", "soc.total_w", "W"),
    ("System power (SMC)", "num", "sys_power_w", "W"),
    ("Temperature sensors", "count", "temperatures", ""),
    ("Fans", "count", "fans", ""),
    ("Battery present", "flag", "battery.present", ""),
    ("Battery voltage", "num", "battery.voltage_mv", "mV"),
    ("Battery capacity", "num", "battery.capacity_wh", "Wh"),
    ("Battery temperature", "num", "battery.temperature_c", "°C"),
    ("Built-in display", "flag", "display.available", ""),
    ("Display brightness", "num", "display.nits", "nits"),
    ("Backlight power (SMC)", "num", "backlight_power_w", "W"),
    ("Keyboard backlight", "num", "keyboard.brightness_pct", "%"),
    ("Audio volume", "num", "audio.volume_pct", "%"),
    ("Wi-Fi connected", "flag", "wifi.connected", ""),
    ("Ethernet connected", "flag", "ethernet.connected", ""),
    ("Network throughput", "num", "network.bytes_in_per_sec", "B/s"),
    ("Disk throughput", "num", "disk.write_bytes_per_sec", "B/s"),
    ("SSD model", "text", "ssd_model", ""),
    ("SSD power", "num", "ssd_power_w", "W"),
    ("Processes listed", "count", "top_processes", ""),
    ("Process power (all)", "num", "all_procs_power_w", "W"),
    ("Per-core CPU usage", "count", "cpu_usage_pct", ""),
    ("CPU tier names", "count", "cpu_tier_names", ""),
    ("GPU cores", "num", "gpu_cores", ""),
    ("Memory installed", "num", "dram_gb", "GB"),
    ("Memory used", "num", "mem_used_gb", "GB"),
    ("Power assertions", "count", "power_assertions", ""),
    ("USB devices", "count", "usb_devices", ""),
    ("Bluetooth devices", "count", "bluetooth_devices", ""),
]

IDENTITY_ROWS = [
    ("macOS", "macos"),
    ("Model", "model"),
    ("Chip", "chip"),
    ("Virtual machine", "vm"),
    ("CPU cores", "cores"),
    ("Memory, GB", "memory_gb"),
    ("`--json` exit code (142 = stopped by the probe)", "json_exit"),
    ("`--dump` exit code", "dump_exit"),
    ("`--dump-smc` exit code", "dump_smc_exit"),
    ("TUI exit code", "tui_exit"),
    ("TUI panicked", "tui_panicked"),
]


def read_text(path):
    return path.read_text(errors="replace") if path.exists() else ""


def read_identity(directory):
    pairs = (line.split("=", 1) for line in read_text(directory / "identity.txt").splitlines())
    return {pair[0]: pair[1] for pair in pairs if len(pair) == 2}


def read_samples(directory):
    """Parse the concatenated JSON objects; a cut-off last object is dropped."""
    text = read_text(directory / "metrics.json")
    decoder = json.JSONDecoder()
    samples, position = [], 0
    while True:
        while position < len(text) and text[position].isspace():
            position += 1
        try:
            sample, position = decoder.raw_decode(text, position)
        except ValueError:
            return samples
        samples.append(sample)


def read_tests(directory):
    """Map test name -> ok / FAILED / ignored, in the order cargo printed them."""
    lines = read_text(directory / "hardware-tests.txt").splitlines()
    matches = (re.match(r"test (\S+) \.\.\. (\w+)", line) for line in lines)
    return {match.group(1): match.group(2) for match in matches if match}


def lookup(sample, path):
    for key in path.split("."):
        if not isinstance(sample, dict):
            return None
        sample = sample.get(key)
    return sample


def cell(samples, kind, path, unit):
    values = [value for value in (lookup(sample, path) for sample in samples) if value is not None]
    if not samples:
        return "no output"
    if not values:
        return "null"
    if kind == "flag":
        return "✅ yes" if any(values) else "no"
    if kind == "text":
        texts = [value for value in values if value]
        return f"✅ {texts[-1]}" if texts else "empty"
    best = max(len(value) for value in values) if kind == "count" else max(values)
    if best <= 0:
        return "0"
    number = f"{best:.2f}".rstrip("0").rstrip(".") if isinstance(best, float) else str(best)
    return f"✅ {number} {unit}".strip()


def true_flags(samples):
    soc = lookup(samples[-1], "soc") if samples else None
    names = [name for name, value in (soc or {}).items() if value is True]
    return ", ".join(names) or "none"


def null_fields(value, path=""):
    """JSON paths holding null: a NaN or infinity that serde_json cannot write."""
    if value is None:
        return [path]
    if isinstance(value, dict):
        return [found for key, item in value.items() for found in null_fields(item, f"{path}.{key}".lstrip("."))]
    if isinstance(value, list):
        return sorted({found for item in value for found in null_fields(item, f"{path}[]")})
    return []


def table(header, rows):
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(row) + " |" for row in rows]
    return "\n".join(lines) + "\n"


def main(directories):
    machines = [
        {
            "identity": read_identity(directory),
            "samples": read_samples(directory),
            "tests": read_tests(directory),
            "channels": len(read_text(directory / "ioreport-channels.txt").splitlines()),
            "smc_keys": len(read_text(directory / "smc-keys.txt").splitlines()),
            "name": directory.name,
        }
        for directory in directories
    ]
    names = [machine["identity"].get("runner", machine["name"]) for machine in machines]

    print("## Machines\n")
    rows = [[label] + [machine["identity"].get(key, "—") for machine in machines] for label, key in IDENTITY_ROWS]
    rows.append(["JSON samples captured"] + [str(len(machine["samples"])) for machine in machines])
    rows.append(["`--dump` lines (IOReport channels)"] + [str(machine["channels"]) for machine in machines])
    rows.append(["`--dump-smc` lines (SMC keys)"] + [str(machine["smc_keys"]) for machine in machines])
    print(table([""] + names, rows))

    print("## Data obtained\n")
    print("✅ = a real value was reported; `0`, `no`, `empty` = reported but empty; `null` = NaN in the output.\n")
    rows = [[label] + [cell(machine["samples"], kind, path, unit) for machine in machines] for label, kind, path, unit in ROWS]
    rows.append(["SoC source flags"] + [true_flags(machine["samples"]) for machine in machines])
    rows.append(
        ["Fields that are null"]
        + [", ".join(null_fields(machine["samples"][-1])) or "none" if machine["samples"] else "no output" for machine in machines]
    )
    print(table(["Metric"] + names, rows))

    print("## Hardware tests (`cargo test --test integration`)\n")
    test_names = list(dict.fromkeys(name for machine in machines for name in machine["tests"]))
    marks = {"ok": "✅", "FAILED": "❌", "ignored": "ignored"}
    rows = [[f"`{name}`"] + [marks.get(machine["tests"].get(name), "—") for machine in machines] for name in test_names]
    print(table(["Test"] + names, rows) if rows else "No test output found.\n")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main([Path(argument) for argument in sys.argv[1:]])
