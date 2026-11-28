#!/usr/bin/env bash
# Run a built macpow on this machine and record what it obtained.
#
#   probe.sh <macpow binary> <output directory> <label>
#
# Every step is time-boxed and none of them fails the script: the point is to
# see what works here, so exit codes are written to identity.txt instead.
# coverage.py turns the output directory into a report.
set -u

binary=$1
out=$2
label=$3
mkdir -p "$out"

# Run a command for at most N seconds (exit code 142 means it was stopped).
bounded() { perl -e 'alarm shift; exec @ARGV' "$@"; }

note() { echo "$1" >> "$out/identity.txt"; }

note "runner=$label"
note "macos=$(sw_vers -productVersion) ($(sw_vers -buildVersion))"
note "model=$(sysctl -n hw.model)"
note "chip=$(sysctl -n machdep.cpu.brand_string)"
note "vm=$(sysctl -n kern.hv_vmm_present)"
note "cores=$(sysctl -n hw.ncpu)"
note "memory_gb=$(($(sysctl -n hw.memsize) / 1073741824))"

# --json never exits by itself: let it print samples, then stop it.
bounded 15 "$binary" --json --interval 1000 > "$out/metrics.json" 2> "$out/metrics.stderr"
note "json_exit=$?"

bounded 60 "$binary" --dump > "$out/ioreport-channels.txt" 2>&1
note "dump_exit=$?"

bounded 60 "$binary" --dump-smc > "$out/smc-keys.txt" 2>&1
note "dump_smc_exit=$?"

# The TUI needs a terminal: give it a pseudo-terminal of a fixed size, let it
# draw for a few seconds, then press "q".
perl -e 'select(undef, undef, undef, 5); print "q"' |
    bounded 30 script -q "$out/tui.typescript" sh -c 'stty rows 50 cols 160; exec "$0"' "$binary" > /dev/null 2> "$out/tui.stderr"
note "tui_exit=$?"
if grep -q panicked "$out/tui.typescript" "$out/tui.stderr" 2> /dev/null; then
    note "tui_panicked=yes"
else
    note "tui_panicked=no"
fi

cat "$out/identity.txt"
