# Contributing

Thanks for helping make Rust and OpenWrt work better together.

## Before opening a change

Open an issue for substantial API changes or new protocol surface so the wire
behavior and compatibility plan can be agreed on first. Small fixes, tests, and
documentation improvements can go directly to a pull request.

Please keep the public crate `no_std + alloc`, preserve the transport boundary,
and avoid coupling protocol code to a particular socket, executor, or logging
implementation.

## Local checks

The repository pins its Rust toolchain. On Linux:

```sh
rustup show
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build -p ubusd --features testing
cargo test --workspace --all-features
cargo package -p ubus-zero
```

For strict differential testing, build the pinned OpenWrt fixtures and require
every reference-backed case to run:

```sh
tools/ubusd-builder/build.sh
UBUS_ZERO_COMPAT=require cargo test --workspace --all-features
```

The compatibility builder needs Docker Buildx. It writes only ignored artifacts
under `.dep/`.

## Pull requests

- Add focused tests for behavior changes.
- Update `CHANGELOG.md` for user-visible changes.
- Keep commits reviewable; Conventional Commit prefixes such as `fix:`,
  `feat:`, and `docs:` make release intent clear.
- Never record golden transcripts against unpinned upstream revisions.
- Do not commit credentials, generated binaries, `.dep/`, or `target/`.

All contributions are licensed under the Apache License, Version 2.0.
