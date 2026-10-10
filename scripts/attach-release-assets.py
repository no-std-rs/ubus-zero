#!/usr/bin/env python3
"""Upload missing release assets and refuse to overwrite different existing bytes."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import quote


def gh(arguments):
    return subprocess.run(["gh", *arguments], check=True, stdout=subprocess.PIPE).stdout


def attach(repository, tag, directory):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("expected OWNER/REPOSITORY")
    if not re.fullmatch(r"ubus-zero-v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", tag):
        raise ValueError("expected a stable ubus-zero release tag")
    version = tag.removeprefix("ubus-zero-v")
    files = [Path(directory) / f"ubus-zero-{version}.cargo-upload", Path(directory) / "SHA256SUMS"]
    release = json.loads(gh(["api", f"repos/{repository}/releases/tags/{quote(tag, safe='')}"]))
    if release["tag_name"] != tag or release["draft"] or release["prerelease"]:
        raise ValueError("expected the matching published stable release")
    existing = {asset["name"]: asset for asset in release["assets"]}
    missing = []
    # Check every existing asset before any new write. A previous partial upload
    # is safe to resume only when its retained bytes match this exact build.
    for path in files:
        local = path.read_bytes()
        asset = existing.get(path.name)
        if asset is None:
            missing.append(str(path))
            continue
        expected = hashlib.sha256(local).hexdigest()
        if asset["size"] != len(local):
            raise ValueError(f"existing asset differs: {path.name}")
        digest = asset.get("digest")
        if digest is None:
            # Older GitHub assets may lack the server-side digest field.
            retained = gh(["api", f"repos/{repository}/releases/assets/{asset['id']}",
                           "--header", "Accept: application/octet-stream"])
            digest = "sha256:" + hashlib.sha256(retained).hexdigest()
        if digest != "sha256:" + expected:
            raise ValueError(f"existing asset differs: {path.name}")
    if missing:
        gh(["release", "upload", tag, *missing, "--repo", repository])
    print(f"Release assets verified: {tag}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: attach-release-assets.py TAG OUTPUT_DIRECTORY")
    try:
        attach(os.environ["GITHUB_REPOSITORY"], sys.argv[1], sys.argv[2])
    except (ValueError, KeyError) as error:
        raise SystemExit(str(error)) from None
