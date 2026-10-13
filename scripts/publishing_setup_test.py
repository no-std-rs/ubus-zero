"""Exercise Runl setup against publishing metadata without registry credentials."""

import contextlib
import copy
import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "publishing_setup", Path(__file__).with_name("configure-runnerless-publishing.py"))
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


class PublishingSetupTest(unittest.TestCase):
    def setUp(self):
        self.repository = {"id": "repo-id", "full_name": setup.REPOSITORY, "state": "adopted"}
        self.target = {
            "registry": "crates", "packages": ["ubus-zero"], "tag_prefix": "old-v",
            "package_tag_prefixes": {"ubus-zero": "old-v"},
            "organization_credential": False, "options": {}, "package_credentials": [],
        }
        self.production = None
        self.calls = []

    def api(self, method, path, body=None):
        self.calls.append((method, path, body))
        if method == "GET" and path.endswith("/repositories"):
            return {"repositories": [self.repository]}
        if method == "GET" and path.endswith("/publishing"):
            return copy.deepcopy({"registries": [self.target], "production": self.production})
        if method == "PUT" and path.endswith("/publishing/crates/target"):
            self.target.update(body)
            return self.target
        if method == "POST" and path.endswith("/environments"):
            self.production = {"id": "production-id", "enabled": False}
            return copy.deepcopy(self.production)
        if method == "PATCH" and path.endswith("/environments/production-id"):
            self.production.update(body)
            return self.production
        self.fail(f"unexpected API call: {method} {path}")

    def configure(self, **options):
        with patch.object(setup, "api", side_effect=self.api), contextlib.redirect_stdout(io.StringIO()):
            return setup.configure("org-id", **options)

    def test_preview_does_not_change_destination_or_production(self):
        before = copy.deepcopy(self.target)
        plan = self.configure(dry_run=True, enable_production=True, organization_credential=True)
        self.assertEqual(plan["target"]["packages"], ["ubus-zero", "ubusd", "ubus-client"])
        self.assertEqual(self.target, before)
        self.assertIsNone(self.production)
        self.assertTrue(all(method == "GET" for method, _, _ in self.calls))

    def test_configures_shared_release_and_leaves_new_production_disabled(self):
        self.configure()
        self.assertEqual(self.target["packages"], list(setup.PACKAGES))
        self.assertEqual(self.target["tag_prefix"], "ubus-zero-v")
        self.assertEqual(self.target["package_tag_prefixes"], {})
        self.assertFalse(self.production["enabled"])
        self.assertFalse(self.target["organization_credential"])
        self.calls.clear()
        self.configure()
        self.assertTrue(all(method == "GET" for method, _, _ in self.calls))

    def test_explicitly_enables_production_and_preserves_shared_credential_binding(self):
        self.configure(organization_credential=True, enable_production=True)
        self.assertTrue(self.production["enabled"])
        self.calls.clear()
        self.configure()
        self.assertTrue(self.target["organization_credential"])
        self.assertTrue(all(method == "GET" for method, _, _ in self.calls))

    def test_paused_repository_is_not_modified(self):
        self.repository["state"] = "paused"
        with self.assertRaisesRegex(SystemExit, "must be active"):
            self.configure(enable_production=True)
        self.assertTrue(all(method == "GET" for method, _, _ in self.calls))

    def test_disabled_production_with_stored_credentials_reports_waiting(self):
        self.production = {"id": "production-id", "enabled": False}
        self.target["package_credentials"] = [
            {"package": name, "status": "environment_disabled"} for name in setup.PACKAGES
        ]
        output = io.StringIO()
        with patch.object(setup, "api", side_effect=self.api), contextlib.redirect_stdout(output):
            setup.configure("org-id")
        self.assertIn("Publishing is waiting for credentials or enabled Production.", output.getvalue())
        self.assertFalse(self.production["enabled"])

    def test_failed_readback_is_reported(self):
        original = self.api

        def ignore_target_update(method, path, body=None):
            if method == "PUT":
                return {}
            return original(method, path, body)

        with patch.object(setup, "api", side_effect=ignore_target_update):
            with self.assertRaisesRegex(SystemExit, "Saved publishing target differs"):
                setup.configure("org-id")


if __name__ == "__main__":
    unittest.main()
