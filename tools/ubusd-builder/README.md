# ubusd-builder

Docker recipe that builds the OpenWrt `ubusd` daemon and `ubus` CLI from
upstream source so the in-tree wire-protocol implementation can be
exercised against a real reference peer.

## Why

The fixture crate `ubus-testkit` exposes `UbusdHarness::start()`, which
spawns a real broker on a scratch socket so the `broker_matrix!` tests
can cross-check our `UnixUbusTransport` / `UbusClient` / `UbusObject`
subset against an upstream reference peer. `start()` returns
`Ok(None)` when no binary is found, so the suite stays hermetic by
default; populating `.dep/bin/ubusd` switches the matrix on.

Building from source (rather than packaging a distro apk/deb, neither
of which exists outside OpenWrt) keeps the test deterministic and lets
us pin specific upstream revisions.

## Build

From the repo root:

```sh
./tools/ubusd-builder/build.sh
```

This invokes `docker buildx build --target export --output
type=local,dest=.dep/bin`, which produces:

```
.dep/bin/ubusd
.dep/bin/ubus
.dep/bin/libubox.so
.dep/bin/libubus.so
.dep/bin/libblobmsg_json.so
.dep/bin/libjson-c.so.5
.dep/bin/libjson-c.so      (symlink)
```

`rpath=$ORIGIN` is baked into the binaries, so the `.so` files sitting
next to them is enough — the loader resolves them without
`LD_LIBRARY_PATH` at test time.

## Target architecture

`build.sh` defaults to the host arch — `linux/arm64` on Apple Silicon
or aarch64 Linux, `linux/amd64` on x86-64 Linux. That matches a
host-arch test runner (native `cargo` on Linux, or `tools/cargo.sh`
paired with `tools/build-cross-image.sh` on macOS arm64). Override
with `UBUSD_PLATFORM` when the runner arch differs from the host —
notably Apple Silicon + stock cross-rs, which runs the test binary
inside a `linux/amd64` container, so set `UBUSD_PLATFORM=linux/amd64`
unless you've also built the host-native cross image:

```sh
UBUSD_PLATFORM=linux/amd64 ./tools/ubusd-builder/build.sh
```

See `tools/build-cross-image.sh` (and cross-rs/cross#751) for the
arm64-host story.

## Pinning upstream revisions

The Dockerfile takes `LIBUBOX_REV` and `UBUS_REV` build-args (default
`master`). To pin to a known-good commit:

```sh
docker buildx build \
    --platform linux/amd64 \
    --build-arg LIBUBOX_REV=<sha> \
    --build-arg UBUS_REV=<sha> \
    --target export \
    --output type=local,dest=.dep/bin \
    -f tools/ubusd-builder/Dockerfile \
    tools/ubusd-builder
```

## Running the tests

```sh
./tools/ubusd-builder/build.sh           # one-time, ~30–60 s
cargo test -p ubus-testkit               # exercises UbusdHarness
cargo test -- --ignored                  # full broker matrix
```

`UbusdHarness::start()` looks for the binary in this order:
`$UBUS_ZERO_UBUSD` (override) → `.dep/bin/ubusd` → `PATH`. When none
of those resolve, the matrix's `_ubusd` variants stay dormant and the
suite still passes — CI without Docker stays green.
