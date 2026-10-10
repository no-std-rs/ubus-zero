# Publishing

Runnerless's embedded `workflow().releasePlease()` manages release PRs, tags and
GitHub Releases. Its package publisher sends verified release assets to crates.io.
GitHub Actions builds/tests those assets without receiving registry credentials.

| Package | Purpose | Publishing order |
| --- | --- | --- |
| `ubus-zero` (library `ubus`) | Default `no_std + alloc` protocol; optional JSON and Linux Unix transport | 1 |
| `ubusd` | Linux daemon and embeddable broker | 2 |
| `ubus-client` | Installable Linux CLI-only crate | 3 |

These packages share [version.txt](../version.txt), release tags of the form
`ubus-zero-vMAJOR.MINOR.PATCH`, and a single release. Versions are independent of
Cellctl. [release_packages.py](../scripts/release_packages.py) lists the public
packages; CI checks that Cargo and the Runnerless request agree.

`ubus-testkit` remains private. The local `crates/libubox-sys` compatibility shim
also remains private while its consolidation is decided: the existing public
[libubox-sys](https://crates.io/crates/libubox-sys) at 0.2.0 comes from
[pawelchcki/libubox](https://github.com/pawelchcki/libubox) and has a broader API.
Publishing this shim as that package requires preserving its API and choosing
a new version.

## Embedded release-please workflow

The default-branch [Runnerless program](../.runnerless-ci.ts) uses
[.ci-toolkit.yml](../.ci-toolkit.yml) to propose version/changelog changes in a
release PR. Merging the release PR creates its tag and release through the App.

The release event starts [the asset build](../.github/workflows/publish-release.yml).
It checks that the event revision and checkout match the tag, validates the
release tooling, aligns public manifests, their internal dependency requirements
and lock entries with version.txt, and tests the workspace. Private package
versions stay unchanged.

Cargo packages and verifies each public crate in dependency order against the
`runnerless-capture` loopback registry. Its index mirrors crates.io and makes
captured packages available to subsequent package builds. Each crate builds from
its distributable archive, including the daemon/CLI's packaged library dependency.
The server never forwards a publish request.

The job attaches `ubus-zero-VERSION.cargo-upload`, `ubusd-VERSION.cargo-upload`,
`ubus-client-VERSION.cargo-upload`, and `SHA256SUMS` to the release. These are exact
Cargo upload bodies containing metadata and the crate archive.

Runnerless submits `packages.crates` after the matching workflow succeeds and
its stable release exists. It verifies the revision, ancestry, checksums and
package identities. On retry it reconciles registry checksums instead of
replacing published versions. Release planning and publication stay in Runnerless.

## Runnerless destination

Connect `no-std-rs/ubus-zero` in [Runnerless](https://app.runnerlesshq.com), using
the organization bound to the installed `no-std-rs` GitHub account. In Package
publishing:

1. Allow `ubus-zero`, `ubusd`, and `ubus-client` with tag prefix `ubus-zero-v`.
2. Use **Set up publishing** to enter the crates.io token in Runnerless's encrypted
   Production vault. Keep it out of GitHub, repository files, logs and chat.
3. Give it permission to create/update these packages, set an expiration, and
   enable Production.

Missing adoption, destination, credentials or enabled Production leaves the task
waiting until setup is completed. Existing libubox-sys publishing stays with its
current repository pending consolidation.

## History and retries

The existing `ubus-zero-v0.1.0` release contains only the library. Its immutable
assets remain valid. The program reads package selection from the completed
workflow's revision so that initial release can still be retried. Subsequent
embedded release-please releases include all three public workspace packages.

Dispatch asset retries **at the same tag** being packaged:

```sh
gh workflow run publish-release.yml --repo no-std-rs/ubus-zero \
  --ref ubus-zero-vVERSION --field tag=ubus-zero-vVERSION
```

Matching existing assets are verified/skipped; only missing assets are uploaded.
Different existing bytes cause failure. When publication is waiting for setup,
finish setup instead of rebuilding assets.

Inspect packages without publishing to crates.io:

```sh
python3 scripts/check-public-packages.py
python3 -m unittest discover -s scripts -p '*_test.py'
python3 scripts/package-registry-uploads.py dist/registry
(cd dist/registry && sha256sum --check SHA256SUMS)
```

The capture needs network reads of crates.io dependencies. Actual publication
happens in Runnerless. No minimum supported Rust version is claimed until a
stable compatibility matrix exists.

See the [Cargo publishing reference](https://doc.rust-lang.org/cargo/reference/publishing.html).
