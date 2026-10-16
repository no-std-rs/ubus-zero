# Changelog

All notable changes to `ubus-zero` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A `no_std` ESP32-C3 round-display example for live OpenWrt statistics over
  uhttpd's ubus bridge, with read-only rpcd setup and touch-controlled pages.
- Optional portable atomics for shared-buffer transports on targets such as
  ESP32-C3 without native pointer-width atomics.
- Parallel compatibility coverage for ACL identities and reloads, descriptor
  passing, daemon lifecycle, upstream and Rust CLIs, and real rpcd/UCI/file
  consumers, with isolated per-process fixtures.
- Full-workspace upstream drift checks and concurrent fault-profile runs.

### Fixed

- Preserve ubus frame and descriptor boundaries across fragmented and pipelined
  socket reads, including disposal of descriptors outside the header window.
- Keep fragmented Unix client replies buffered until the outer blob header
  arrives instead of prematurely accepting eight-byte header fragments.
- Match pinned OpenWrt ACL file safety, identity resolution, supplementary
  groups, mixed-type entries, and method visibility during lookup, while
  unresolved identity names remain denied.
- Replace duplicate ACL JSON keys using the final value in the original key
  position, including permission revocation and privilege tables.
- Read complete supplementary-group records so long lines cannot truncate GIDs
  into incorrect ACL grants or drop valid groups below the 64-group limit.
- Match ACL grants once per object when filtering non-root lookup replies,
  avoiding repeated registry scans for every declared method.
- Register malformed object signatures as untyped objects like upstream.
- Reject unexpected replies or trailing bytes during golden transcript replay.

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
