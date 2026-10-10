#!/usr/bin/env python3
"""Capture Cargo publish requests locally for Runnerless to relay to crates.io.

The local registry never forwards a publish request. Its sparse index mirrors
crates.io and adds packages captured earlier in this run, so packages can be
prepared using the same dependency index Cargo uses for crates.io.
"""

import hashlib
import hmac
import http.server
import json
import os
from pathlib import Path
import secrets
import struct
import subprocess
import sys
import threading
import urllib.error
import urllib.request
from release_packages import PACKAGES


ROOT = Path(__file__).resolve().parents[1]
MAX_UPLOAD = 32 * 1024 * 1024
MAX_INDEX = 4 * 1024 * 1024


def index_path(name):
    if len(name) == 1:
        return "1/" + name
    if len(name) == 2:
        return "2/" + name
    if len(name) == 3:
        return "3/" + name[0] + "/" + name
    return name[:2] + "/" + name[2:4] + "/" + name


def parse_upload(body, expected_name, version):
    if len(body) < 8 or len(body) > MAX_UPLOAD:
        raise ValueError("invalid Cargo upload size")
    metadata_size = struct.unpack_from("<I", body)[0]
    if not 0 < metadata_size <= 128 * 1024 or len(body) < 8 + metadata_size:
        raise ValueError("invalid Cargo metadata size")
    metadata = json.loads(body[4 : 4 + metadata_size])
    archive_size = struct.unpack_from("<I", body, 4 + metadata_size)[0]
    if not 0 < archive_size <= MAX_UPLOAD or len(body) != 8 + metadata_size + archive_size:
        raise ValueError("invalid Cargo archive size")
    if metadata.get("name") != expected_name or metadata.get("vers") != version:
        raise ValueError("Cargo package identity differs from release")
    archive = body[8 + metadata_size :]
    dependencies = [
        {
            "name": item.get("explicit_name_in_toml") or item["name"],
            "package": item["name"] if item.get("explicit_name_in_toml") else None,
            "req": item["version_req"],
            "features": item["features"],
            "optional": item["optional"],
            "default_features": item["default_features"],
            "target": item["target"],
            "kind": item["kind"],
            "registry": item["registry"],
        }
        for item in metadata["deps"]
    ]
    record = {
        "name": expected_name,
        "vers": version,
        "deps": dependencies,
        "cksum": hashlib.sha256(archive).hexdigest(),
        "features": metadata["features"],
        "yanked": False,
    }
    return (json.dumps(record, separators=(",", ":")) + "\n").encode()


class LocalRegistry(http.server.ThreadingHTTPServer):
    allow_reuse_address = True

    def __init__(self, output, version):
        super().__init__(("127.0.0.1", 0), RegistryHandler)
        self.output = output
        self.version = version
        self.expected = ""
        self.index = {}
        self.error = None
        self.token = secrets.token_hex(32)


class RegistryHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/index/config.json":
            data = json.dumps(
                {
                    "dl": f"http://127.0.0.1:{self.server.server_port}/crates",
                    "api": f"http://127.0.0.1:{self.server.server_port}",
                }
            ).encode()
        elif self.path.startswith("/crates/"):
            parts = self.path.split("/")
            if len(parts) != 5 or parts[4] != "download":
                self.send_error(404)
                return
            name, version = parts[2:4]
            asset = self.server.output / f"{name}-{version}.cargo-upload"
            if name in PACKAGES and version == self.server.version and asset.is_file():
                body = asset.read_bytes()
                offset = 8 + struct.unpack_from("<I", body)[0]
                data = body[offset:]
            else:
                try:
                    with urllib.request.urlopen(f"https://static.crates.io/crates/{name}/{name}-{version}.crate", timeout=20) as response:
                        data = response.read(MAX_UPLOAD + 1)
                except (OSError, TimeoutError):
                    self.send_error(503)
                    return
                if len(data) > MAX_UPLOAD:
                    self.send_error(413)
                    return
        elif self.path.startswith("/index/"):
            item = self.path.removeprefix("/index/")
            data = self.server.index.get(item)
            if data is None:
                try:
                    with urllib.request.urlopen(
                        "https://index.crates.io/" + item, timeout=20
                    ) as response:
                        data = response.read(MAX_INDEX + 1)
                except urllib.error.HTTPError as error:
                    self.send_error(error.code)
                    return
                except (OSError, TimeoutError):
                    self.send_error(503)
                    return
                if len(data) > MAX_INDEX:
                    self.send_error(413)
                    return
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_PUT(self):
        if self.path != "/api/v1/crates/new" or not self.server.expected:
            self.send_error(404)
            return
        if not hmac.compare_digest(self.headers.get("Authorization", ""), self.server.token):
            self.send_error(403)
            return
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= MAX_UPLOAD:
                raise ValueError("invalid upload length")
            body = self.rfile.read(size)
            record = parse_upload(body, self.server.expected, self.server.version)
            self.server.output.joinpath(self.server.expected + "-" + self.server.version + ".cargo-upload").write_bytes(body)
            self.server.index[index_path(self.server.expected)] = record
        except (ValueError, KeyError, TypeError, OSError) as error:
            self.server.error = str(error)
            self.send_error(422)
            return
        data = b'{"warnings":{"invalid_categories":[],"invalid_badges":[],"other":[]}}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, _format, *_args):
        pass


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: package-registry-uploads.py OUTPUT_DIRECTORY")
    version = ROOT.joinpath("version.txt").read_text().strip()
    if not version or not all(part.isdigit() for part in version.split(".")) or len(version.split(".")) != 3:
        raise SystemExit("version.txt must contain a stable semantic version")
    output = Path(sys.argv[1]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    server = LocalRegistry(output, version)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        # Cargo's existing cache remains available; only these scoped config
        # arguments redirect resolution and publication for this process.
        index = f"sparse+http://127.0.0.1:{server.server_port}/index/"
        environment = os.environ.copy()
        environment["CARGO_REGISTRIES_RUNNERLESS_CAPTURE_INDEX"] = index
        environment["CARGO_REGISTRIES_RUNNERLESS_CAPTURE_TOKEN"] = server.token
        for name in PACKAGES:
            server.expected = name
            command = [
                "cargo",
                "--config", 'source.crates-io.replace-with="local-mirror"',
                "--config", f'source.local-mirror.registry="{index}"',
                "publish", "--registry", "runnerless-capture", "--allow-dirty", "--locked",
                "-p", name,
            ]
            result = subprocess.run(command, cwd=ROOT, env=environment, timeout=120, check=False)
            asset = output / f"{name}-{version}.cargo-upload"
            if result.returncode or not asset.is_file() or server.error:
                raise RuntimeError(f"could not package {name}: {server.error or result.returncode}")
        digest_lines = [
            f"{hashlib.sha256((output / f'{name}-{version}.cargo-upload').read_bytes()).hexdigest()}  {name}-{version}.cargo-upload"
            for name in PACKAGES
        ]
        output.joinpath("SHA256SUMS").write_text("\n".join(digest_lines) + "\n")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
