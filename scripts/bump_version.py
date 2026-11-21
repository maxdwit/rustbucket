#!/usr/bin/env python3
"""Change the version of rustbucket in every place that holds it.

    scripts/bump_version.py 0.2.0          explicit version
    scripts/bump_version.py patch|minor|major

Cargo.toml is the source of truth (the server reads it at compile time: Admin > About, `rustbucket version`,
/metrics). The root package entry of Cargo.lock (and fuzz/Cargo.lock) is kept in step so `cargo build --locked` works.
Nothing is committed or tagged. The build number (date, commit) is not part of this: `make deploy` stamps it.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def read_version(text):
    m = re.search(r'(?ms)^\[package\].*?^version\s*=\s*"([^"]+)"', text)
    if not m:
        sys.exit("no version in the [package] section of Cargo.toml")
    return m.group(1)


def lock_entry(text):
    return re.search(r'(name = "rustbucket"\nversion = ")([^"]+)(")', text)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    cargo = ROOT / "Cargo.toml"
    ctext = cargo.read_text()
    old = read_version(ctext)
    m = SEMVER.match(old)
    if not m:
        sys.exit(f"current version {old!r} is not X.Y.Z")
    major, minor, patch = map(int, m.groups())
    arg = sys.argv[1]
    new = {"major": f"{major + 1}.0.0", "minor": f"{major}.{minor + 1}.0", "patch": f"{major}.{minor}.{patch + 1}"}.get(arg, arg)
    if not SEMVER.match(new):
        sys.exit(f"{arg!r} is neither X.Y.Z nor patch/minor/major")
    if new == old:
        sys.exit(f"already at {old}")

    changed = []
    cargo.write_text(re.sub(r'(?ms)(^\[package\].*?^version\s*=\s*")[^"]+(")', lambda mm: mm.group(1) + new + mm.group(2), ctext, count=1))
    changed.append("Cargo.toml")
    for rel in ("Cargo.lock", "fuzz/Cargo.lock"):
        p = ROOT / rel
        if not p.exists():
            continue
        t = p.read_text()
        e = lock_entry(t)
        if not e:
            print(f"warning: no rustbucket entry in {rel}", file=sys.stderr)
            continue
        if e.group(2) != old:
            print(f"warning: {rel} had {e.group(2)}, expected {old}; set to {new}", file=sys.stderr)
        p.write_text(t[: e.start(2)] + new + t[e.end(2):])
        changed.append(rel)
    print(f"{old} -> {new}   ({', '.join(changed)})")
    print("not committed. A build is stamped with its date and commit by `make deploy`.")


if __name__ == "__main__":
    main()
