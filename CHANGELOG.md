# Changelog

All notable changes to `ubus-zero` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-10-13

### Added

- Installable `ubus-client` with discovery, calls, events, subscriptions,
  monitoring, deadlines, JSON Lines batches and shell-backed object providers.
- Optional `json` and Linux `unix` library adapters extracted from Cellctl.
- Client APIs for object signatures, multiple replies and buffered asynchronous
  traffic.
- Embedded Runnerless release-please publishing for the public workspace crates.
- Runl setup for the three-crate publishing destination and Production environment.

### Changed

- Expose the Rust broker as the `ubusd` crate and executable.
- Support explicit OpenWrt library/header discovery for packaged FFI bindings.

## [0.1.0] - 2026-07-29

### Added

- Initial public release of the pure-Rust `no_std + alloc` ubus protocol
  library.
- Synchronous object lookup and method invocation.
- Object registration, method dispatch, replies, and removal.
- OpenWrt-compatible wire, blob, blobmsg, and envelope codecs.
- Runtime-neutral transport trait.
- Differential compatibility suite backed by a private Rust broker and pinned
  OpenWrt reference builds.

[Unreleased]: https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.2.0...HEAD
[0.2.0]: https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.1.0...ubus-zero-v0.2.0
[0.1.0]: https://github.com/no-std-rs/ubus-zero/releases/tag/ubus-zero-v0.1.0
