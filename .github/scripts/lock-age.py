#!/usr/bin/env python3
"""Fail if Cargo.lock pins a crates.io version published less than N days ago.

    python3 -I .github/scripts/lock-age.py [Cargo.lock] [days]

Cargo's own `registry.global-min-publish-age` (see .cargo/config.toml) only
filters versions while it resolves; a version that is already in Cargo.lock
stays, however it got there (a Dependabot security update, which skips the
cooldown, or `cargo update --precise`). This check closes that gap.

Publication times come from the crates.io sparse index ("pubtime"). A version
without one was published before crates.io started recording it, so it is old.
A locked version the index does not list at all is an error. Dependencies from
git or another registry cannot be checked here and get a warning.
"""

import json
import sys
import time
import tomllib
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
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
    """Map every published version of `name` to its pubtime (or None)."""
    request = urllib.request.Request(index_url(name), headers={"User-Agent": "lock-age-check"})
    for attempt in range(1, attempts + 1):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                entries = [json.loads(line) for line in response.read().decode().splitlines()]
            return {entry["vers"]: entry.get("pubtime") for entry in entries}
        except urllib.error.HTTPError as error:
            # 404 and other client errors will not change on a retry.
            if error.code < 500 or attempt == attempts:
                raise
        except OSError:
            # A timeout or a dropped connection.
            if attempt == attempts:
                raise
        time.sleep(attempt)


def main(lock_path="Cargo.lock", days="7"):
    with open(lock_path, "rb") as lock_file:
        packages = tomllib.load(lock_file)["package"]
    cutoff = datetime.now(timezone.utc) - timedelta(days=int(days))

    # Packages without a source are this workspace's own crates.
    from_crates_io = [p for p in packages if p.get("source") == CRATES_IO]
    for package in packages:
        if package.get("source") not in (None, CRATES_IO):
            print(f"::warning::{package['name']} {package['version']} comes from "
                  f"{package['source']}; its age cannot be checked")

    names = sorted({p["name"] for p in from_crates_io})
    with ThreadPoolExecutor(max_workers=8) as pool:
        times = dict(zip(names, pool.map(publish_times, names)))

    failures = 0
    for package in from_crates_io:
        versions = times[package["name"]]
        label = f"{package['name']} {package['version']}"
        if package["version"] not in versions:
            print(f"::error::{label} is not in the crates.io index")
            failures += 1
            continue
        published = versions[package["version"]]
        if published and datetime.fromisoformat(published.replace("Z", "+00:00")) > cutoff:
            print(f"::error::{label} was published {published}, less than {days} days ago")
            failures += 1

    if failures:
        return 1
    print(f"All {len(from_crates_io)} crates.io packages in {lock_path} are at least {days} days old.")
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
