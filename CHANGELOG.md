# Changelog

All notable changes to `ubus-zero` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/pawelchcki/ubus-zero/compare/ubus-zero-v0.1.0...HEAD
[0.1.0]: https://github.com/pawelchcki/ubus-zero/releases/tag/ubus-zero-v0.1.0
