#!/usr/bin/env python3
"""Fail if Cargo.lock pins a crates.io version published less than N days ago.

    python3 -I .github/scripts/lock-age.py [Cargo.lock] [days]

Cargo's own `registry.global-min-publish-age` (see .cargo/config.toml) only
filters versions while it resolves; a version that is already in Cargo.lock
stays, however it got there (a Dependabot security update, which skips the
cooldown, or `cargo update --precise`). This check closes that gap.

Publication times come from the crates.io sparse index ("pubtime"). A version
without one was published before crates.io started recording it, so it is old.
"""

import json
import sys
import tomllib
import urllib.request
from datetime import datetime, timedelta, timezone

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def index_url(name):
    name = name.lower()
    if len(name) <= 2:
        return f"https://index.crates.io/{len(name)}/{name}"
    if len(name) == 3:
        return f"https://index.crates.io/3/{name[0]}/{name}"
    return f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"


def publish_times(name, attempts=3):
    request = urllib.request.Request(index_url(name), headers={"User-Agent": "lock-age-check"})
    for attempt in range(1, attempts + 1):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                entries = [json.loads(line) for line in response.read().decode().splitlines()]
            return {entry["vers"]: entry.get("pubtime") for entry in entries}
        except OSError:
            # A timeout or a dropped connection: try again, then give up loudly.
            if attempt == attempts:
                raise


def main(lock_path="Cargo.lock", days="7"):
    with open(lock_path, "rb") as lock_file:
        packages = [p for p in tomllib.load(lock_file)["package"] if p.get("source") == CRATES_IO]
    cutoff = datetime.now(timezone.utc) - timedelta(days=int(days))

    too_young = []
    times = {}
    for package in packages:
        if package["name"] not in times:
            times[package["name"]] = publish_times(package["name"])
        published = times[package["name"]].get(package["version"])
        if published and datetime.fromisoformat(published.replace("Z", "+00:00")) > cutoff:
            too_young.append(f"{package['name']} {package['version']} (published {published})")

    if too_young:
        print(f"::error::{len(too_young)} locked crate version(s) are younger than {days} days:")
        print("\n".join(f"  {line}" for line in too_young))
        return 1
    print(f"All {len(packages)} crates.io packages in {lock_path} are at least {days} days old.")
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
