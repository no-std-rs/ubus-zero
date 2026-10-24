# Changelog

All notable changes to `ubus-zero` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Enable the Unix transport, signal helpers, broker polling and peer credentials
  on Android using the existing Linux implementation.
- Keep Android group lookups compatible with API 21 and use Bionic's errno
  accessor in the standalone broker's signal handler.

## [0.4.1](https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.4.0...ubus-zero-v0.4.1) (2026-10-18)


### Bug Fixes

* preserve empty keys in high-level JSON conversion ([#38](https://github.com/no-std-rs/ubus-zero/issues/38)) ([d11c9ee](https://github.com/no-std-rs/ubus-zero/commit/d11c9ee56dcd2fa78a1ef7313ce97b9f6f7541ba))
* reject oversized replies before provider transmission ([#37](https://github.com/no-std-rs/ubus-zero/issues/37)) ([d2f6942](https://github.com/no-std-rs/ubus-zero/commit/d2f6942dea490e9dbe33c4285c4f3d8d154d04ab))
* apply envelope policy to typed inbound messages ([#36](https://github.com/no-std-rs/ubus-zero/issues/36)) ([b0bd286](https://github.com/no-std-rs/ubus-zero/commit/b0bd286f2dc1ced86b6fa66e387fdeb695c0d44b))
* retire object routes during path replacement ([#35](https://github.com/no-std-rs/ubus-zero/issues/35)) ([7cee88e](https://github.com/no-std-rs/ubus-zero/commit/7cee88e999721346ffe0147f1797738473185879))
* reject I/O after an abandoned partial Unix send ([#34](https://github.com/no-std-rs/ubus-zero/issues/34)) ([d497282](https://github.com/no-std-rs/ubus-zero/commit/d497282f3d00a96d8d58289ada2a4192c6ac88b1))

### Performance Improvements

* share NOTIFY fan-out templates across subscribers ([#39](https://github.com/no-std-rs/ubus-zero/issues/39)) ([fffe3b0](https://github.com/no-std-rs/ubus-zero/commit/fffe3b0294ee32e8e65fd899e544dfc1ba16eb4e))

## [0.4.0](https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.3.0...ubus-zero-v0.4.0) (2026-10-18)


### Features

* bound broker memory and add allocator-free submission ([#29](https://github.com/no-std-rs/ubus-zero/issues/29)) ([227d7b4](https://github.com/no-std-rs/ubus-zero/commit/227d7b48a63609b6ec02a1f402a14c285e1028f8))

### Bug Fixes

* return standard ubus provider error statuses ([#33](https://github.com/no-std-rs/ubus-zero/issues/33)) ([2247961](https://github.com/no-std-rs/ubus-zero/commit/2247961f5312031c1e54170b5b2c0b867da47842))
* match reply origins and preserve interleaved messages ([#32](https://github.com/no-std-rs/ubus-zero/issues/32)) ([3d217a5](https://github.com/no-std-rs/ubus-zero/commit/3d217a5762bfea63c2f42e78d8037c561079b77f))
* preserve shared types and sibling subscription cleanup ([#31](https://github.com/no-std-rs/ubus-zero/issues/31)) ([0583c8f](https://github.com/no-std-rs/ubus-zero/commit/0583c8fbe5f8fbd26a7283311a9f6f283830ac37))
* honor exact outer ubus frame lengths ([#30](https://github.com/no-std-rs/ubus-zero/issues/30)) ([8a7b95e](https://github.com/no-std-rs/ubus-zero/commit/8a7b95e4f93b718e6c4173b3c6f2a7589a8100d2))
* preserve ubus parity and remove broker bottlenecks ([#28](https://github.com/no-std-rs/ubus-zero/issues/28)) ([1200873](https://github.com/no-std-rs/ubus-zero/commit/120087321b16a55bccf67c28b6aa710e316dddb3))

### Performance Improvements

* reduce large-message broker receive overhead ([#26](https://github.com/no-std-rs/ubus-zero/issues/26)) ([6030e3b](https://github.com/no-std-rs/ubus-zero/commit/6030e3bf7e9532ce7ba32553db9bdd803a27c439))

### Added

- Allocator-free borrowed codecs and fixed-buffer envelope/blobmsg builders;
  disable the new default `alloc` feature to link without a global allocator.
  Existing users of owned APIs with `default-features = false` must enable
  `alloc` explicitly (JSON/Unix/portable-atomic features imply it).
- Linux bounded signal-handler submission and a resumable allocation-free
  stream sender, with atomic whole-frame rejection on queue saturation.
- Post-burst idle-peer memory benchmark, allocation-trapped real-signal tests
  and a freestanding C3-target link check without an allocator.
- Broker CPU/RSS counters, trial uncertainty intervals, compiler-profile and
  allocation diagnostics, and verified registry/ACL/idle-peer/fanout benchmarks.
- A verified 32-request pipelined ping benchmark to cover broker batching, and
  same-machine comparisons of C, baseline Rust and current Rust using a Git ref.
- Reproducible Rust/OpenWrt broker benchmarks with verified protocol workloads,
  latency percentiles, throughput, raw samples and standalone comparison reports.

### Fixed

- Report positive ubus provider statuses: INVALID_ARGUMENT (2),
  METHOD_NOT_FOUND (3) and UNKNOWN_ERROR (9), replacing private negative codes.
- Require broker-origin replies for provider registration and acknowledged removal.
- Match client replies by both sequence and object, preserve interleaved events
  and invocation replies, and avoid reusing live sequences across wraparound.
- Bound outstanding split-phase client invocations to 256 and retire them on
  completion or receive errors; raw transport reads bypass this tracking.
- Honor shared object-type IDs (including pathless registrations), release the
  signature after the final object is removed.
- Preserve sibling-subscription cleanup after one subscriber object unsubscribes.
- Decode the exact outer ubus length without waiting for stream padding, and
  preserve the next frame and descriptor after an unaligned outer blob.
- Allocate per-peer byte buffers lazily and recycle empty receive/transmit
  buffers through bounded broker-wide spares, preserving partial input and FDs.
- Verify relay descriptor closure with per-pipe EOF instead of process-wide FD
  counts that race parallel tests.
- Reset the request idle budget on accepted DATA so long object listings do
  not time out while progressing; silence and unrelated traffic remain bounded.
- Bound named-object history to live registrations and preserve last-valid
  duplicate protocol attributes like the pinned OpenWrt parser.
- Use Linux epoll for active-peer dispatch and optimize the host daemon at
  release level 3 while keeping the public no_std library size-oriented.
- Reduce broker credential copies, frame allocations, buffer compaction and
  descriptor-free reply syscalls; index path/owner/event lookups and reuse
  NOTIFY encoding while retaining FD, monitor and backpressure behavior.

- Reduce large-message broker receive syscalls with a shared adaptive buffer,
  while preserving batching and descriptor boundaries.

## [0.3.0](https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.2.0...ubus-zero-v0.3.0) (2026-10-16)


### Features

* publish C3 firmware and add a GitHub Pages USB flasher ([#21](https://github.com/no-std-rs/ubus-zero/issues/21)) ([89422c0](https://github.com/no-std-rs/ubus-zero/commit/89422c0c9e36b25e2de9bd9279e9167f304f4177))
* add no_std ESP32-C3 OpenWrt router dashboard ([#20](https://github.com/no-std-rs/ubus-zero/issues/20)) ([8babfbb](https://github.com/no-std-rs/ubus-zero/commit/8babfbbe3b341250989affa1097ad83c27b2db92))

### Bug Fixes

* **ci:** isolate Cargo artifacts for concurrent fault profiles ([#23](https://github.com/no-std-rs/ubus-zero/issues/23)) ([051b507](https://github.com/no-std-rs/ubus-zero/commit/051b507b5a41a7207385381e498202516bb8094d))
* **release:** capture released versions without mirroring their crates.io entries ([#18](https://github.com/no-std-rs/ubus-zero/issues/18)) ([36109d2](https://github.com/no-std-rs/ubus-zero/commit/36109d260675c6d0af0b680ab15944e856519bd2))

### Added

- Public ESP32-C3 factory firmware builds, Runnerless artifact publication, and
  a GitHub Pages USB flasher with browser-local configuration and checksums.
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

[Unreleased]: https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.4.1...HEAD
[0.2.0]: https://github.com/no-std-rs/ubus-zero/compare/ubus-zero-v0.1.0...ubus-zero-v0.2.0
[0.1.0]: https://github.com/no-std-rs/ubus-zero/releases/tag/ubus-zero-v0.1.0
