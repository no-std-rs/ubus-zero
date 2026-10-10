"""Release identity and upload validation must fail before mutating package files."""

import importlib.util
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).parent))


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = load("prepare-release")
uploads = load("package-registry-uploads")
assets = load("attach-release-assets")


class ReleaseIdentityTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        original = prepare.ROOT
        self.addCleanup(setattr, prepare, "ROOT", original)
        prepare.ROOT = Path(self.directory.name)
        self.root = prepare.ROOT
        self.root.joinpath("version.txt").write_text("0.2.0\n")
        for name, relative in prepare.PACKAGES.items():
            manifest = self.root / relative
            manifest.parent.mkdir(parents=True, exist_ok=True)
            dependencies = '\n[dependencies]\nubus = { package = "ubus-zero", path = "../..", version = "0.1.0" }\n' if name in ("ubusd", "ubus-client") else ''
            manifest.write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\n' + dependencies)
        private = self.root / "crates/testkit/Cargo.toml"
        private.parent.mkdir(parents=True, exist_ok=True)
        private.write_text('[package]\nname = "ubus-testkit"\nversion = "0.1.0"\n[dependencies]\nlibubox-sys = { path = "../libubox-sys", version = "0.1.0" }\n')
        self.root.joinpath("Cargo.lock").write_text(
            ''.join(f'[[package]]\nname = "{name}"\nversion = "0.1.0"\n' for name in prepare.PACKAGES) +
            '[[package]]\nname = "ubus-testkit"\nversion = "0.1.0"\n')

    def test_aligns_public_packages_and_dependency_requirements(self):
        prepare.prepare("ubus-zero-v0.2.0")
        self.assertIn('version = "0.2.0"', self.root.joinpath("Cargo.toml").read_text())
        self.assertIn('name = "ubus-testkit"\nversion = "0.1.0"', self.root.joinpath("Cargo.lock").read_text())
        for relative in prepare.PACKAGES.values():
            manifest = self.root / relative
            self.assertEqual(prepare.tomllib.loads(manifest.read_text())["package"]["version"], "0.2.0")
        self.assertEqual(prepare.tomllib.loads(self.root.joinpath("crates/ubusd/Cargo.toml").read_text())["dependencies"]["ubus"]["version"], "0.2.0")
        self.assertEqual(prepare.tomllib.loads(self.root.joinpath("crates/testkit/Cargo.toml").read_text())["package"]["version"], "0.1.0")

    def test_rejects_wrong_tag_without_writes(self):
        before = self.root.joinpath("Cargo.toml").read_bytes()
        for tag in ["v0.2.0", "ubus-zero-v0.1.0", "ubus-zero-v0.2.0-rc.1", "ubus-zero-v00.2.0"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                prepare.prepare(tag)
        self.assertEqual(before, self.root.joinpath("Cargo.toml").read_bytes())

    def test_invalid_lock_leaves_manifest_intact(self):
        before = self.root.joinpath("Cargo.toml").read_bytes()
        self.root.joinpath("Cargo.lock").write_text("")
        with self.assertRaises(ValueError):
            prepare.prepare("ubus-zero-v0.2.0")
        self.assertEqual(before, self.root.joinpath("Cargo.toml").read_bytes())


class UploadIdentityTest(unittest.TestCase):
    def body(self, name="ubus-zero", version="0.2.0"):
        metadata = json.dumps({"name": name, "vers": version, "deps": [], "features": {}}).encode()
        archive = b"archive"
        return struct.pack("<I", len(metadata)) + metadata + struct.pack("<I", len(archive)) + archive

    def test_index_checksum_is_for_archive(self):
        record = json.loads(uploads.parse_upload(self.body(), "ubus-zero", "0.2.0"))
        self.assertEqual(record["cksum"], uploads.hashlib.sha256(b"archive").hexdigest())

    def test_wrong_identity_and_truncated_body_are_refused(self):
        for body in [self.body("ubus-testkit"), self.body(version="0.1.0"), self.body()[:-1], self.body() + b"extra", b"short"]:
            with self.subTest(body=body), self.assertRaises(ValueError):
                uploads.parse_upload(body, "ubus-zero", "0.2.0")


class AssetRetryTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.names = [f"{name}-0.2.0.cargo-upload" for name in prepare.PACKAGES] + ["SHA256SUMS"]
        self.contents = [name.encode() for name in self.names]
        for name, data in zip(self.names, self.contents):
            self.root.joinpath(name).write_bytes(data)
        self.release = {"tag_name": "ubus-zero-v0.2.0", "draft": False, "prerelease": False, "assets": []}

    def retained(self, index, data=None):
        data = self.contents[index] if data is None else data
        return {"id": index + 1, "name": self.names[index], "size": len(data),
                "digest": "sha256:" + assets.hashlib.sha256(data).hexdigest()}

    def attach(self, responses):
        with patch.object(assets, "gh", side_effect=responses) as remote:
            assets.attach("no-std-rs/ubus-zero", "ubus-zero-v0.2.0", self.root)
        return remote

    def test_partial_upload_resumes_only_missing_checksum(self):
        self.release["assets"] = [self.retained(i) for i in range(len(prepare.PACKAGES))]
        remote = self.attach([json.dumps(self.release).encode(), b""])
        self.assertEqual(remote.call_args_list[-1].args[0], ["release", "upload", "ubus-zero-v0.2.0",
                         str(self.root / "SHA256SUMS"), "--repo", "no-std-rs/ubus-zero"])

    def test_completed_upload_performs_no_writes(self):
        self.release["assets"] = [self.retained(i) for i in range(len(self.names))]
        remote = self.attach([json.dumps(self.release).encode()])
        self.assertEqual(remote.call_count, 1)

    def test_conflicting_checksum_prevents_uploading_missing_package(self):
        self.release["assets"] = [self.retained(len(self.names) - 1, b"different")]
        with patch.object(assets, "gh", return_value=json.dumps(self.release).encode()) as remote:
            with self.assertRaisesRegex(ValueError, "existing asset differs"):
                assets.attach("no-std-rs/ubus-zero", "ubus-zero-v0.2.0", self.root)
        self.assertEqual(remote.call_count, 1)

    def test_missing_digest_downloads_to_verify_without_replacing(self):
        retained = self.retained(0)
        retained["digest"] = None
        self.release["assets"] = [retained] + [self.retained(i) for i in range(1, len(self.names))]
        remote = self.attach([json.dumps(self.release).encode(), self.contents[0]])
        self.assertEqual(remote.call_count, 2)
        self.assertEqual(remote.call_args_list[-1].args[0][:2], ["api", "repos/no-std-rs/ubus-zero/releases/assets/1"])


if __name__ == "__main__":
    unittest.main()
