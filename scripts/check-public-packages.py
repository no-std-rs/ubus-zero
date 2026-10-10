#!/usr/bin/env python3
"""Check that Cargo and Runnerless agree on public package names and order."""

import json
from pathlib import Path
import re
import subprocess
from release_packages import PACKAGES

ROOT = Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT))
public = {p["name"] for p in metadata["packages"] if p["publish"] != []}
if public != set(PACKAGES):
    raise SystemExit(f"public package mismatch: Cargo={sorted(public)}, release={sorted(PACKAGES)}")
program = ROOT.joinpath(".runnerless-ci.ts").read_text()
configured = re.findall(r'\.add\("([a-z0-9-]+)",', program)
if configured != list(PACKAGES):
    raise SystemExit(f"Runnerless package order differs: {configured}")
for package in metadata["packages"]:
    if package["name"] in PACKAGES:
        if not {"crates-io", "runnerless-capture"}.issubset(package["publish"]):
            raise SystemExit(f"unexpected publish allowlist for {package['name']}")
print("Cargo and embedded Runnerless publishing agree: " + ", ".join(PACKAGES))
