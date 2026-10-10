#!/usr/bin/env python3
"""Align public manifests, dependency requirements and lock entries for a release."""

from pathlib import Path
import re
import sys
import tomllib
from release_packages import PACKAGES

ROOT = Path(__file__).resolve().parents[1]
VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"


def prepare(tag):
    if not re.fullmatch("ubus-zero-v" + VERSION, tag):
        raise ValueError("expected a stable ubus-zero-vMAJOR.MINOR.PATCH release tag")
    version = tag.removeprefix("ubus-zero-v")
    if ROOT.joinpath("version.txt").read_text().strip() != version:
        raise ValueError("tag differs from version.txt")
    updates = {}
    manifests = list(PACKAGES.values())
    for relative in manifests:
        manifest = ROOT / relative
        source = manifest.read_text()
        parsed = tomllib.loads(source)
        if relative in PACKAGES.values():
            if PACKAGES.get(parsed["package"]["name"]) != relative:
                raise ValueError(f"unexpected package name in {relative}")
            source, count = re.subn(r'(?m)^version\s*=\s*"[^"\n]+"', f'version = "{version}"', source)
            if count != 1:
                raise ValueError(f"expected one package version in {relative}")

        def dependency(match):
            name, body = match[1], match[2]
            fields = tomllib.loads("dependency = {" + body + "}")["dependency"]
            if fields.get("package", name) in PACKAGES and "path" in fields and "version" in fields:
                body = re.sub(r'version\s*=\s*"[^"\n]+"', f'version = "{version}"', body)
            return name + " = {" + body + "}"

        source = re.sub(r'(?m)^([A-Za-z0-9_-]+)\s*=\s*\{([^\n]+)\}', dependency, source)
        updates[manifest] = source
    lock = ROOT / "Cargo.lock"
    locked = lock.read_text()
    for name in PACKAGES:
        locked, count = re.subn(r'(\[\[package\]\]\nname = "' + re.escape(name) + r'"\n)version = "[^"\n]+"',
                               lambda match: match[1] + f'version = "{version}"', locked)
        if count != 1:
            raise ValueError(f"expected one {name} entry in Cargo.lock")
    updates[lock] = locked
    # Validate every manifest and lock entry before writing any file.
    for path, source in updates.items():
        path.write_text(source)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: prepare-release.py ubus-zero-vMAJOR.MINOR.PATCH")
    try:
        prepare(sys.argv[1])
    except ValueError as error:
        raise SystemExit(str(error)) from None
