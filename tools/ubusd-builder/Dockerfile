# syntax=docker/dockerfile:1.6
#
# Builds OpenWrt's `ubusd` daemon, `ubus` and `uci` CLIs, and `rpcd`
# from upstream source
# so `ubus-testkit::UbusdHarness` can cross-check our wire subset
# against a real reference broker.
#
# Linkage: dynamic against glibc (musl was tried first but produced
# binaries the Fedora host couldn't load without bundling the musl
# loader). Exported binaries carry `rpath=$ORIGIN` so the .so files
# alongside the binary resolve without LD_LIBRARY_PATH.

# bullseye (glibc 2.31) so the produced binaries run in the
# `cross` aarch64-unknown-linux-gnu container (Ubuntu 20.04, glibc 2.31)
# without "version `GLIBC_2.34' not found" loader errors. Bumping the
# base means rebuilding artefacts to a newer glibc — only do that when
# every consumer (CI runners, dev containers) catches up.
FROM debian:bullseye-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        dpkg-dev \
        git \
        pkg-config \
        libjson-c-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Pinned so upstream drift can't silently change what the cross-impl
# matrix verifies. Bump deliberately, then re-run
# tools/ubusd-builder/build.sh and the test suite.
ARG LIBUBOX_REV=17f527f
ARG UBUS_REV=24864e7
ARG UCI_REV=74f6277
ARG RPCD_REV=28faf64

WORKDIR /src
RUN git clone https://git.openwrt.org/project/libubox.git libubox \
 && git -C libubox checkout "$LIBUBOX_REV" \
 && git clone https://git.openwrt.org/project/ubus.git ubus \
 && git -C ubus checkout "$UBUS_REV" \
 && git clone https://git.openwrt.org/project/uci.git uci \
 && git -C uci checkout "$UCI_REV" \
 && git clone https://git.openwrt.org/project/rpcd.git rpcd \
 && git -C rpcd checkout "$RPCD_REV"

# libubox first — ubus links it.
WORKDIR /src/libubox/build
RUN cmake \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DBUILD_LUA=OFF \
        -DBUILD_EXAMPLES=OFF \
        -DCMAKE_INSTALL_RPATH='$ORIGIN' \
        -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
        .. \
 && make -j"$(nproc)" install

WORKDIR /src/ubus/build
RUN cmake \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DBUILD_LUA=OFF \
        -DBUILD_EXAMPLES=OFF \
        -DCMAKE_INSTALL_RPATH='$ORIGIN' \
        -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
        .. \
 && make -j"$(nproc)" install

WORKDIR /src/uci/build
RUN cmake \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DBUILD_LUA=OFF \
        -DBUILD_EXAMPLES=OFF \
        -DCMAKE_INSTALL_RPATH='$ORIGIN' \
        -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
        .. \
 && make -j"$(nproc)" install

# iwinfo (wireless) and ucode (scripting) plugins need libraries the
# image doesn't carry, and rpc-sys uses strlcpy (glibc >= 2.38, absent
# on bullseye); the session/ACL bridge tests only exercise the built-in
# session/uci tables plus the file plugin.
WORKDIR /src/rpcd/build
RUN cmake \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DIWINFO_SUPPORT=OFF \
        -DUCODE_SUPPORT=OFF \
        -DRPCSYS_SUPPORT=OFF \
        -DCMAKE_INSTALL_RPATH='$ORIGIN' \
        -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON \
        .. \
 && make -j"$(nproc)" install

# Stage everything in /export so the scratch stage emits a flat layout.
# `-L` chases the .so symlinks so the export directory is self-contained
# (no dangling links back to /usr/local/lib). Resolve the json-c
# multiarch path from `dpkg-architecture` rather than hardcoding
# x86_64-linux-gnu, so a buildx rebuild on aarch64 still works.
RUN set -eux; \
    MULTIARCH="$(dpkg-architecture -qDEB_HOST_MULTIARCH)"; \
    JSONC_REAL="$(readlink -f "/usr/lib/${MULTIARCH}/libjson-c.so")"; \
    mkdir -p /export; \
    cp    /usr/local/sbin/ubusd                /export/ubusd; \
    cp    /usr/local/bin/ubus                  /export/ubus; \
    cp    /usr/local/bin/uci                   /export/uci; \
    cp    /usr/local/sbin/rpcd                 /export/rpcd; \
    cp -L /usr/local/lib/libubox.so            /export/libubox.so; \
    cp -L /usr/local/lib/libubus.so            /export/libubus.so; \
    cp -L /usr/local/lib/libuci.so             /export/libuci.so; \
    cp -L /usr/local/lib/libblobmsg_json.so    /export/libblobmsg_json.so; \
    cp -L "$JSONC_REAL"                        /export/libjson-c.so.5; \
    ln -s libjson-c.so.5 /export/libjson-c.so; \
    mkdir -p /export/include; \
    cp -r /usr/local/include/libubox       /export/include/libubox; \
    cp    /usr/local/include/libubus.h     /export/include/libubus.h; \
    cp    /usr/local/include/ubusmsg.h     /export/include/ubusmsg.h; \
    cp    /usr/local/include/ubus_common.h /export/include/ubus_common.h; \
    cp    /usr/local/include/uci*.h        /export/include/; \
    mkdir -p /export/rpcd-plugins; \
    cp    /usr/local/lib/rpcd/*.so          /export/rpcd-plugins/

# Surface the runtime linkage in the build log so a missing .so dep
# (transitive json-c upgrade, debian package rename, …) is visible.
RUN echo '--- ubusd linkage ---'; ldd /export/ubusd  || true; \
    echo '--- ubus linkage ---';  ldd /export/ubus   || true; \
    echo '--- uci linkage ---';   ldd /export/uci    || true; \
    echo '--- rpcd linkage ---';  ldd /export/rpcd   || true

FROM scratch AS export
COPY --from=builder /export/ /
