# Publishing `ubus-zero`

Only the root `ubus-zero` package is public. `ubusd-rs`, `ubus-testkit`, and
`libubox-sys` are compatibility infrastructure and have `publish = false`.

| Field | Value |
| --- | --- |
| crates.io package | `ubus-zero` |
| Rust library target | `ubus` |
| GitHub repository | `no-std-rs/ubus-zero` |
| Version source | [`version.txt`](../version.txt) |
| Release tag | `ubus-zero-vMAJOR.MINOR.PATCH` |
| Asset build workflow | [`publish-release.yml`](../.github/workflows/publish-release.yml) |
| Registry publisher | Runnerless, enabled Production environment |

Versions are independent from Cellctl and every other “Zero” library.

## Automated releases

The trusted default-branch [Runnerless program](../.runnerless-ci.ts) calls
`releasePlease()` using the policy in [`.ci-toolkit.yml`](../.ci-toolkit.yml).
Runnerless proposes version and changelog updates in a release PR. Merging that
PR creates its tag and GitHub Release through the installed GitHub App.

The release event starts a credentialless GitHub Actions build. It aligns only
the root Cargo manifest and lockfile entry with `version.txt`, tests the workspace,
and verifies `cargo package`. Cargo then sends its exact registry upload body to
the explicitly allowed `runnerless-capture` loopback registry; that local server
never forwards uploads. The private crates remain unpublishable. The job attaches
`ubus-zero-VERSION.cargo-upload` and `SHA256SUMS` to the GitHub Release.

Runnerless submits `packages.crates` only after the matching asset workflow
completes successfully and the stable GitHub Release exists. The host checks the
release revision, default-branch ancestry, checksums, package identity and Cargo
archive before publishing. It reconciles crates.io's index checksum on retries,
so an already published matching version succeeds without another upload.

Release planning and registry publication use the existing Runnerless service.
GitHub runners build the package assets and run tests; they receive no crates.io
credentials and do not perform registry publication.

## Runnerless enrollment

In [Runnerless](https://app.runnerlesshq.com), connect `no-std-rs/ubus-zero` in
the organization that owns the installed GitHub account. The GitHub App must
have access to this repository, and `no-std-rs` must be bound to that organization.

In the repository's Package publishing panel:

1. Add a crates.io destination allowing only `ubus-zero`, with tag prefix
   `ubus-zero-v`.
2. Use **Set up publishing** to store a dedicated crates.io token in Runnerless's
   encrypted Production vault. Enter it in the password field, never in GitHub,
   repository files, logs, or chat.
3. Scope the token to `ubus-zero`, with `publish-update` and `publish-new` for the
   first release, and set an expiration. Enable Production when prompted.

Missing adoption, destination, credentials or enabled Production leaves the
publish task waiting; completing setup allows it to resume automatically.
No infrastructure allowlist PR, Worker secret, or new cloud resource is needed.

The former release-plz workflow has been removed. If an external trusted
publisher was previously configured on crates.io, retire it before switching
this destination to Runnerless token publishing.

## Initial release and retries

For the first release, create the stable `ubus-zero-v0.1.0` GitHub Release at the
validated main commit containing this program and build workflow. This anchors
Runnerless's subsequent release planning and starts the asset job. The same
Runnerless publishing path handles both new packages and later versions.

Inspect the initial package before creating the release:

```sh
cargo package -p ubus-zero --locked --list
cargo package -p ubus-zero --locked
cargo publish -p ubus-zero --registry crates-io --locked --dry-run
```

Extract the generated `.crate` into a clean temporary directory and build it
independently. Published versions are permanent.

To retry an asset build, dispatch the workflow **at the same tag** being packaged:

```sh
gh workflow run publish-release.yml --repo no-std-rs/ubus-zero \
  --ref ubus-zero-v0.1.0 --field tag=ubus-zero-v0.1.0
```

The job checks that its GitHub revision is the tag's checked-out revision, so
Runnerless reads the completed build's version even after `main` advances.
Matching existing assets are verified and skipped; only missing assets are uploaded.
Different existing bytes cause failure and are never overwritten. If the publish
task is waiting for setup, finish Runnerless setup instead of rebuilding assets.

No minimum supported Rust version is claimed for `0.1.0`; `rust-version`
remains unset until a stable compatibility matrix exists.

See the [Cargo publishing reference](https://doc.rust-lang.org/cargo/reference/publishing.html).
