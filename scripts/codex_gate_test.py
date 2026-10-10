"""Codex activation must preserve protections and refuse unavailable publishers."""

import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("enable-codex-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class CodexGateTest(unittest.TestCase):
    def setUp(self):
        self.ruleset = {
            "id": 1, "name": "Protect main", "target": "branch", "enforcement": "active",
            "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
            "bypass_actors": [],
            "rules": [
                {"type": "deletion"}, {"type": "non_fast_forward"},
                {"type": "pull_request", "parameters": {
                    "required_review_thread_resolution": True, "allowed_merge_methods": ["squash", "rebase"],
                }},
                {"type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": True, "do_not_enforce_on_create": False,
                    "required_status_checks": [{"context": "Stable workspace"}, {"context": "Pinned OpenWrt parity"}],
                }},
            ],
        }
        self.pull = {"state": "open", "base": {"ref": "main", "repo": {"full_name": gate.REPO}},
                     "head": {"sha": "a" * 40}}
        self.checks = [{"name": gate.CONTEXT, "app": {"id": gate.APP_ID}, "head_sha": "a" * 40}]
        self.writes = []
        self.reads = 0

    def github(self, path, *, body=None, paginate=False):
        if body is not None:
            self.writes.append(copy.deepcopy(body))
            self.ruleset.update(copy.deepcopy(body))
            return copy.deepcopy(self.ruleset)
        if path == "":
            return {"default_branch": "main"}
        if path == "pulls/13":
            return self.pull
        if path.startswith("commits/"):
            return [{"check_runs": []}, {"check_runs": self.checks}]
        if path.startswith("rulesets?"):
            return [[dict(self.ruleset, source_type="Repository", source=gate.REPO)]]
        if path == "rulesets/1":
            self.reads += 1
            return copy.deepcopy(self.ruleset)
        raise AssertionError(path)

    def run_main(self, *options):
        with patch.object(gate, "github", side_effect=self.github), patch("sys.argv", ["gate", "13", *options]), patch("builtins.print"):
            gate.main()

    def test_adds_only_app_pinned_gate_without_mutating_input(self):
        before = copy.deepcopy(self.ruleset)
        body = gate.require_gate(self.ruleset)
        self.assertEqual(self.ruleset, before)
        added = body["rules"][-1]["parameters"]["required_status_checks"].pop()
        self.assertEqual(added, {"context": gate.CONTEXT, "integration_id": gate.APP_ID})
        self.assertEqual(body, {key: before[key] for key in body})

    def test_activates_and_preserves_existing_rules(self):
        expected = gate.require_gate(copy.deepcopy(self.ruleset))
        self.run_main()
        self.assertEqual(self.writes, [expected])

    def test_dry_run_does_not_write(self):
        self.run_main("--dry-run")
        self.assertEqual(self.writes, [])

    def test_refuses_missing_wrong_app_or_stale_check(self):
        for checks in [[], [dict(self.checks[0], app={"id": 15368})],
                       [dict(self.checks[0], head_sha="b" * 40)]]:
            with self.subTest(checks=checks), self.assertRaises(SystemExit):
                self.checks = checks
                self.run_main()
            self.assertEqual(self.writes, [])

    def test_refuses_closed_or_wrong_base_pr(self):
        original = copy.deepcopy(self.pull)
        for pull in [dict(original, state="closed"), dict(original, base={"ref": "develop"})]:
            with self.subTest(pull=pull), self.assertRaises(SystemExit):
                self.pull = pull
                self.run_main()
            self.assertEqual(self.writes, [])

    def test_refuses_policy_changed_during_inspection(self):
        original = self.github

        def changed(path, **kwargs):
            result = original(path, **kwargs)
            if path == "rulesets/1" and self.reads == 2:
                result["rules"].append({"type": "required_signatures"})
            return result

        with patch.object(gate, "github", side_effect=changed), patch("sys.argv", ["gate", "13"]):
            with self.assertRaises(SystemExit):
                gate.main()
        self.assertEqual(self.writes, [])

    def test_existing_app_pinned_check_is_idempotent(self):
        self.ruleset.update(gate.require_gate(copy.deepcopy(self.ruleset)))
        self.assertEqual(gate.require_gate(self.ruleset)["rules"], self.ruleset["rules"])

    def test_refuses_changed_policy_or_wrong_publisher_requirement(self):
        before = copy.deepcopy(self.ruleset)
        self.ruleset["conditions"]["ref_name"]["include"] = ["refs/heads/develop"]
        with self.assertRaises(SystemExit):
            gate.require_gate(self.ruleset)
        self.ruleset = before
        self.ruleset["rules"][-1]["parameters"]["required_status_checks"].append({"context": gate.CONTEXT})
        with self.assertRaises(SystemExit):
            gate.require_gate(self.ruleset)


if __name__ == "__main__":
    unittest.main()
