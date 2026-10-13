#!/usr/bin/env python3
"""Configure the public workspace crates in Runnerless through an authorized Runl."""

import argparse
import json
import os
import subprocess
from urllib.parse import quote

from release_packages import PACKAGES

REPOSITORY = "no-std-rs/ubus-zero"
TAG_PREFIX = "ubus-zero-v"


def api(method, path, body=None):
    result = subprocess.run(
        ["runl", "api", method, path],
        input=json.dumps(body) if body is not None else None,
        text=True, capture_output=True, check=True,
    )
    return json.loads(result.stdout)


def configure(organization, *, dry_run=False, organization_credential=False,
              enable_production=False):
    base = f"/api/v1/orgs/{quote(organization, safe='')}/repositories"
    repositories = api("GET", base)["repositories"]
    matches = [repo for repo in repositories if repo["full_name"] == REPOSITORY]
    if len(matches) != 1:
        raise SystemExit(f"Adopt {REPOSITORY} first with runl repositories add {REPOSITORY}.")
    repository = matches[0]
    if repository["state"] not in ("adopted", "active"):
        raise SystemExit("The repository must be active in Runnerless before configuring publishing.")
    base += "/" + quote(repository["id"], safe="")
    current = api("GET", base + "/publishing")
    target = next((item for item in current["registries"] if item["registry"] == "crates"), {})
    desired = {
        "packages": list(PACKAGES),
        "tag_prefix": TAG_PREFIX,
        "package_tag_prefixes": {},
        "organization_credential": organization_credential or target.get("organization_credential", False),
        "options": {},
    }
    production = current["production"]
    plan = {
        "repository": REPOSITORY,
        "target": desired,
        "create_production": production is None,
        "enable_production": enable_production,
    }
    if dry_run:
        print(json.dumps(plan, indent=2))
        return plan
    if any(target.get(key) != value for key, value in desired.items()):
        api("PUT", base + "/publishing/crates/target", desired)
    if production is None:
        production = api("POST", base + "/environments", {"name": "production"})
    if enable_production and not production["enabled"]:
        api("PATCH", base + "/environments/" + quote(production["id"], safe=""), {"enabled": True})
    actual = api("GET", base + "/publishing")
    saved = next((item for item in actual["registries"] if item["registry"] == "crates"), {})
    if any(saved.get(key) != value for key, value in desired.items()):
        raise SystemExit("Saved publishing target differs from the requested configuration.")
    if (actual["production"] is None
            or (enable_production and not actual["production"]["enabled"])):
        raise SystemExit("Production configuration differs from the requested configuration.")
    print(json.dumps({"repository": REPOSITORY, "production": actual["production"],
                      "crates": saved}, indent=2))
    credentials = {item["package"]: item["status"] for item in saved["package_credentials"]}
    if any(credentials.get(name) != "configured" for name in PACKAGES):
        print("Publishing is waiting for credentials or enabled Production. "
              f"Use runl publishing token {repository['id']} crates --enable-production.")
    return actual


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--organization", default=os.environ.get("RUNL_ORGANIZATION"),
                        help="organization ID (defaults to the Runl login's RUNL_ORGANIZATION)")
    parser.add_argument("--dry-run", action="store_true", help="inspect the proposed configuration without writing")
    parser.add_argument("--organization-credential", action="store_true",
                        help="explicitly bind the destination to the organization's crates.io token")
    parser.add_argument("--enable-production", action="store_true", help="enable Production for release publication")
    args = parser.parse_args()
    if not args.organization:
        parser.error("supply --organization ID or run inside runl auth login -- command")
    configure(args.organization, dry_run=args.dry_run,
              organization_credential=args.organization_credential,
              enable_production=args.enable_production)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.strip() or "Runl API request failed.") from None
