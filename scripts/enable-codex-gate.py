#!/usr/bin/env python3
"""Require an observed Runnerless Codex check while preserving the main ruleset."""

import argparse
import copy
import json
import subprocess

REPO = "no-std-rs/ubus-zero"
CONTEXT = "Runnerless / Codex review"
APP_ID = 4602759
RULESET = "Protect main"


def github(path, *, body=None, paginate=False):
    command = ["gh", "api", f"repos/{REPO}/{path}".rstrip("/")]
    if paginate:
        command.extend(["--paginate", "--slurp"])
    if body is not None:
        command.extend(["--method", "PUT", "--input", "-"])
    result = subprocess.run(
        command, input=json.dumps(body) if body is not None else None,
        text=True, capture_output=True, check=True,
    )
    return json.loads(result.stdout)


def require_gate(ruleset):
    body = {key: copy.deepcopy(ruleset[key]) for key in (
        "name", "target", "enforcement", "bypass_actors", "conditions", "rules",
    )}
    conditions = body["conditions"].get("ref_name", {})
    if (body["name"] != RULESET or body["target"] != "branch"
            or body["enforcement"] != "active"
            or conditions.get("include") != ["~DEFAULT_BRANCH"]
            or conditions.get("exclude") != []):
        raise SystemExit("Protect main no longer targets only the default branch; inspect its rules first.")
    rules = [rule for rule in body["rules"] if rule["type"] == "required_status_checks"]
    if len(rules) != 1:
        raise SystemExit("Expected one existing required-status-check rule; inspect Protect main first.")
    checks = rules[0]["parameters"]["required_status_checks"]
    matches = [check for check in checks if check["context"] == CONTEXT]
    if matches and matches != [{"context": CONTEXT, "integration_id": APP_ID}]:
        raise SystemExit("The Codex check already has a different publisher requirement; inspect it first.")
    if not matches:
        checks.append({"context": CONTEXT, "integration_id": APP_ID})
    return body


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pr", type=int, help="an open PR with an observed Runnerless Codex check")
    parser.add_argument("--dry-run", action="store_true", help="show the complete proposed ruleset without writing")
    args = parser.parse_args()
    if args.pr <= 0:
        parser.error("PR number must be positive")
    repository = github("")
    pull = github(f"pulls/{args.pr}")
    if (pull["state"] != "open" or pull["base"]["ref"] != repository["default_branch"]
            or pull["base"]["repo"]["full_name"] != REPO):
        raise SystemExit("Use an open pull request targeting this repository's default branch.")
    head = pull["head"]["sha"]
    pages = github(f"commits/{head}/check-runs?filter=all&per_page=100", paginate=True)
    if not any(check["name"] == CONTEXT and check["app"]["id"] == APP_ID
               and check["head_sha"] == head for page in pages for check in page["check_runs"]):
        raise SystemExit(
            "Runnerless has not reported its current-head Codex check. "
            "Merge the program onto main, connect the repository in Runnerless, then retry."
        )
    candidates = [ruleset for page in github("rulesets?includes_parents=false&per_page=100", paginate=True)
                  for ruleset in page if ruleset["name"] == RULESET
                  and ruleset["source_type"] == "Repository" and ruleset["source"] == REPO]
    if len(candidates) != 1:
        raise SystemExit("Expected exactly one repository ruleset named Protect main.")
    path = f"rulesets/{candidates[0]['id']}"
    current = github(path)
    body = require_gate(current)
    if args.dry_run:
        print(json.dumps(body, indent=2))
        return
    if github(path) != current:
        raise SystemExit("The ruleset changed during inspection; retry with its new policy.")
    github(path, body=body)
    actual = github(path)
    if any(actual[key] != value for key, value in body.items()):
        raise SystemExit("The saved ruleset differs from the requested policy; inspect Protect main.")
    print(f"Required {CONTEXT} from App {APP_ID}; existing CI and protection settings preserved.")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.strip() or "GitHub API request failed.") from None
