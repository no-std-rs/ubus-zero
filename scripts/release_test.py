"""Release identity and upload validation must fail before mutating package files."""

import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = load("prepare-release")
uploads = load("package-registry-uploads")


class ReleaseIdentityTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        original = prepare.ROOT
        self.addCleanup(setattr, prepare, "ROOT", original)
        prepare.ROOT = Path(self.directory.name)
        self.root = prepare.ROOT
        self.root.joinpath("version.txt").write_text("0.2.0\n")
        self.root.joinpath("Cargo.toml").write_text('[package]\nname = "ubus-zero"\nversion = "0.1.0"\n')
        self.root.joinpath("Cargo.lock").write_text(
            '[[package]]\nname = "ubus-zero"\nversion = "0.1.0"\n'
            '[[package]]\nname = "ubus-testkit"\nversion = "0.1.0"\n')

    def test_aligns_only_public_package(self):
        prepare.prepare("ubus-zero-v0.2.0")
        self.assertIn('version = "0.2.0"', self.root.joinpath("Cargo.toml").read_text())
        self.assertIn('name = "ubus-testkit"\nversion = "0.1.0"', self.root.joinpath("Cargo.lock").read_text())

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


if __name__ == "__main__":
    unittest.main()
