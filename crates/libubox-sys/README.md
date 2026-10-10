# libubox-sys

Hand-written Linux FFI bindings and C accessors for OpenWrt `libubox` and
`libubus`, from the [ubus-zero workspace](https://github.com/no-std-rs/ubus-zero).
These are a tested subset of the upstream C API. The implementation is pre-1.0.

Build OpenWrt's libraries and headers, then select their directory:

```sh
UBUS_ZERO_LIBUBOX_DIR=/opt/openwrt-libs cargo build
```

The directory must contain `libubox.so`, `libubus.so`, `include/libubus.h` and
`include/libubox/{blob.h,blobmsg.h}`. `libblobmsg_json.so` and
`include/libubox/blobmsg_json.h` additionally enable JSON helpers. A C compiler
(`CC`, default `cc`) and archiver (`AR`, default `ar`) are required. The build
probes C structure sizes, so the probe must execute on the build host.

An explicitly selected directory fails the build if its libraries, headers or
probes are invalid. Without an explicit directory, the workspace's `.dep/bin`
fixtures are used when available. Otherwise the crate builds with
`libubox_sys::AVAILABLE == false` and omits the unavailable FFI symbols. Check
`AVAILABLE` when constructing optional compatibility tests.

The workspace fixture builder exports the expected layout. The resulting
binaries link to these shared libraries; keep them available at runtime.
