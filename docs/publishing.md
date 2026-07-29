# Publishing `ubus-zero`

Only the root `ubus-zero` package is public. `ubusd-rs`, `ubus-testkit`, and
`libubox-sys` are compatibility infrastructure and have `publish = false`.

## Release identity

| Field | Value |
| --- | --- |
| crates.io package | `ubus-zero` |
| Rust library target | `ubus` |
| GitHub repository | `pawelchcki/ubus-zero` |
| Release workflow | `release-plz.yml` |
| GitHub environment | `crates-io` |
| Tag format | release-plz workspace default (`ubus-zero-v0.1.0`) |

Versions are independent from Cellctl and every other “Zero” library.

## Automated releases after 0.1.0

[`release-plz.toml`](../release-plz.toml) disables release management by
default and opts in only `ubus-zero`. On ordinary pushes to `main`,
[`release-plz.yml`](../.github/workflows/release-plz.yml) creates or updates a
release PR. Publishing runs only for a merged `release-plz-*` PR or a manual
retry dispatched from `main`.

The release job:

- has only `contents: write`, `pull-requests: read`, and `id-token: write`;
- enters the reviewer-protected `crates-io` environment;
- exchanges GitHub's OIDC identity through release-plz;
- stores no crates.io token; and
- publishes, tags, and creates the GitHub release.

The repository workflow-token default stays read-only. The PR job declares
only the permissions needed to update its release branch and pull request.
Third-party actions are pinned by full commit SHA.

## Required GitHub controls

The `crates-io` environment must:

1. permit deployments only from `main`;
2. require review by `pawelchcki`;
3. allow self-review so a single-maintainer repository cannot deadlock; and
4. disable administrator bypass.

GitHub Actions may create pull requests, but no crates.io token may exist as a
repository, environment, or organization secret for this project.

## Initial 0.1.0 bootstrap

crates.io trusted publishing cannot create a brand-new package. Version `0.1.0`
therefore needs one temporary local token:

1. Merge the validated repository to a clean public `main` commit.
2. Reconfirm that `ubus-zero` is unclaimed.
3. Create a dedicated crates.io token with only new/update publish capability.
   Keep it local and never copy it into GitHub.
4. Inspect the exact archive:

   ```sh
   cargo package -p ubus-zero --list
   cargo package -p ubus-zero
   ```

5. Extract the generated `.crate` into a clean temporary directory and build
   it independently.
6. Run the full dry-run:

   ```sh
   cargo publish -p ubus-zero --dry-run
   ```

7. Only if every prior check succeeds, publish:

   ```sh
   cargo publish -p ubus-zero
   ```

8. Create the `ubus-zero-v0.1.0` GitHub release at the exact published commit.
9. On crates.io, configure the trusted publisher with these exact values:

   | Field | Value |
   | --- | --- |
   | GitHub owner | `pawelchcki` |
   | Repository | `ubus-zero` |
   | Workflow | `release-plz.yml` |
   | Environment | `crates-io` |

10. Enable trusted-publishing-only mode and revoke the bootstrap token.

Published versions are permanent. Never upload until archive inspection,
independent extraction, and dry-run checks are all green.

No minimum supported Rust version is claimed for `0.1.0`; `rust-version`
remains unset until a stable compatibility matrix exists.

See the [Cargo publishing reference](https://doc.rust-lang.org/cargo/reference/publishing.html)
and [release-plz trusted-publishing guide](https://release-plz.dev/docs/github/quickstart).
