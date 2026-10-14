"""Release identity and upload validation must fail before mutating package files."""

import importlib.util
import json
import hashlib
import os
from pathlib import Path
import subprocess
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

    def test_generated_release_packages_in_real_git_checkout(self):
        def command(*args):
            return subprocess.check_output(args, cwd=self.root, stderr=subprocess.STDOUT)

        for name, relative in prepare.PACKAGES.items():
            manifest = self.root / relative
            dependencies = ('\n[dependencies]\nubus = { package = "ubus-zero", path = "../..", version = "0.1.0" }\n'
                            if name != "ubus-zero" else '')
            manifest.write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
                                'license = "MIT"\ndescription = "Release packaging regression fixture"\n'
                                'publish = ["crates-io", "runnerless-capture"]\n' + dependencies)
            source = manifest.parent / 'src'
            source.mkdir(exist_ok=True)
            source.joinpath('lib.rs').write_text('pub fn value() -> u8 { ' + ('42' if name == 'ubus-zero' else 'ubus::value()') + ' }\n')
        for name in ['testkit', 'libubox-sys']:
            directory = self.root / 'crates' / name
            directory.mkdir(exist_ok=True)
            directory.joinpath('Cargo.toml').write_text(f'[package]\nname = "ubus-{name}"\nversion = "0.1.0"\nedition = "2021"\npublish = false\n')
            directory.joinpath('src').mkdir(exist_ok=True)
            directory.joinpath('src/lib.rs').write_text('pub fn fixture() {}\n')
        with self.root.joinpath('Cargo.toml').open('a') as manifest:
            manifest.write('\n[workspace]\nresolver = "2"\nmembers = ["crates/ubusd", "crates/ubus-client", "crates/testkit", "crates/libubox-sys"]\n')
        self.root.joinpath('version.txt').write_text('0.1.0\n')
        self.root.joinpath('.gitignore').write_text('/target/\n/dist/\n/.cargo-home/\n')
        self.root.joinpath('Cargo.lock').unlink()
        command('cargo', 'generate-lockfile', '--offline')
        command('git', 'init', '-b', 'main')
        command('git', 'add', '.')
        command('git', '-c', 'user.name=Release regression', '-c', 'user.email=release@example.invalid', 'commit', '-m', 'baseline public crates at 0.1.0')
        command('git', 'checkout', '-b', 'release-please--branches--main')
        # These are the two files the release proposal changes.
        self.root.joinpath('version.txt').write_text('0.2.0\n')
        self.root.joinpath('CHANGELOG.md').write_text('# 0.2.0\n\nRelease fixture.\n')
        prepare.prepare('ubus-zero-v0.2.0')
        metadata = json.loads(command('cargo', 'metadata', '--no-deps', '--format-version', '1'))
        public = {package['name']: package['version'] for package in metadata['packages'] if package['publish'] != []}
        self.assertEqual(public, dict.fromkeys(prepare.PACKAGES, '0.2.0'))
        with self.assertRaises(subprocess.CalledProcessError):
            command('cargo', 'package', '-p', 'ubus-zero', '--list')
        command('cargo', 'package', '-p', 'ubus-zero', '--list', '--allow-dirty')
        output = self.root / 'dist/registry'
        with patch.object(uploads, 'ROOT', self.root), patch.object(sys, 'argv', ['package-registry-uploads.py', str(output)]), patch.dict(os.environ, {'CARGO_HOME': str(self.root / '.cargo-home')}):
            uploads.main()
        checksums = output.joinpath('SHA256SUMS').read_text()
        for name in prepare.PACKAGES:
            asset = output / f'{name}-0.2.0.cargo-upload'
            body = asset.read_bytes()
            size = struct.unpack_from('<I', body)[0]
            identity = json.loads(body[4:4 + size])
            self.assertEqual((identity['name'], identity['vers']), (name, '0.2.0'))
            self.assertIn(hashlib.sha256(body).hexdigest() + '  ' + asset.name, checksums)
        for name in ['testkit', 'libubox-sys']:
            manifest = prepare.tomllib.loads(self.root.joinpath(f'crates/{name}/Cargo.toml').read_text())
            self.assertEqual(manifest['package']['version'], '0.1.0')
        locked = prepare.tomllib.loads(self.root.joinpath('Cargo.lock').read_text())['package']
        self.assertEqual({package['name']: package['version'] for package in locked if package['name'] not in prepare.PACKAGES},
                         {'ubus-testkit': '0.1.0', 'ubus-libubox-sys': '0.1.0'})


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
