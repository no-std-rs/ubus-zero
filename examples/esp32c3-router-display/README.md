# ESP32-C3 OpenWrt router display

Bare-metal **`no_std + alloc` Rust** for the round gadget from
[`pawelchcki/razem-tag`](https://github.com/pawelchcki/razem-tag), inspected at
[`c9bd439`](https://github.com/pawelchcki/razem-tag/tree/c9bd439959b311bc16fd3fe44942d473badc6313).
It connects to an OpenWrt router over 2.4 GHz Wi-Fi and renders five live pages
on the built-in screen. Tap to advance; pages also advance every eight seconds.

| Page | Statistics |
| --- | --- |
| Traffic | WAN download/upload in Mb/s, 60-sample scrolling graph |
| System | 1/5/15-minute load averages, used/total RAM, root filesystem free/total, uptime |
| Wi-Fi | Selected AP's SSID, channel, signal, quality, associated stations |
| Counters | WAN received/sent bytes, packets, errors, drops |
| Router | Hostname, model, OpenWrt version, WAN state/device/IPv4/link uptime |

System and WAN polls target a one-second interval. Wi-Fi is sampled every five
polls; board identity every sixty. Requests have five-second deadlines, so a
slow router reduces the sampling rate. The UI runs independently and marks
failed or old samples as stale. Missing fields show `--`. Load averages are
Linux runnable-task load, not CPU utilization percentages. Wi-Fi signal and
quality describe the selected interface; AP drivers can leave them unavailable.

## Hardware reused from the animation project

GUITION **ESP32-2424S012-V1.0**, ESP32-C3, 240×240 round GC9A01 LCD and
CST816-series touch controller:

| Function | GPIO |
| --- | --- |
| LCD SPI clock / MOSI | 6 / 7 |
| LCD DC / CS / backlight | 2 / 10 / 3 |
| Touch SDA / SCL / reset | 4 / 5 / 1 |

The LCD has no separate reset GPIO; its init sequence starts with software
reset and uses this panel's BGR order and inversion. CS stays low. Touch is
polled because its interrupt shares GPIO10 with LCD CS. SPI runs at 40 MHz.
Rendering uses one 16-row tile (7.5 KiB), yields between tiles, and refreshes
about four times per second. No full framebuffer, ESP-IDF runtime, C application
code, or standard library is required. Espressif's radio firmware comes through
`esp-radio`, as in the source project. This example replaces the animation
firmware; it does not import its Matter commissioning or effects.

## Router setup

The ESP cannot connect to the router's local Unix-domain ubus socket directly.
This example uses the normal **uhttpd `/ubus` JSON-RPC bridge** and rpcd login.
Requests and replies use `ubus-zero`'s `json` / blobmsg codecs; it does not use
the synchronous native-socket `UbusClient`. No raw ubusd TCP port or companion
computer is needed.

On OpenWrt, install **rpcd** and **uhttpd-mod-ubus** with the router's package
manager. Optionally install **rpcd-mod-iwinfo** for Wi-Fi statistics. Beryl and
Beryl AX vendor images can differ in installed packages and interface names;
inspect the actual router before selecting `WAN_INTERFACE` and `WIFI_DEVICE`:

```sh
ubus list 'network.interface.*'
ubus call network.interface.wan status
ubus call iwinfo devices
```

Use the active logical uplink (`wan`, `wwan`, `tethering`, etc.). The firmware
automatically follows its `l3_device`, including PPPoE, when reading counters.
Download is RX on that WAN device and upload is TX. These are link counters,
not per-client usage, Wi-Fi PHY rates, or a speed test; offloaded traffic may
not be counted by every driver. VPN-tunnel traffic requires choosing the
corresponding logical interface. An IPv6-only WAN can have no IPv4 address.

Copy the `router/` directory onto the router. Generate a password hash on your
computer with `openssl passwd -6` (it prompts for the password), then on the
router run:

```sh
export DASHBOARD_PASSWORD_HASH='$6$PASTE_THE_COMPLETE_HASH_HERE'
sh router/setup.sh
unset DASHBOARD_PASSWORD_HASH
```

The script installs [dashboard.json](router/dashboard.json), creates an rpcd
login named `dashboard`, and starts a dedicated uhttpd instance bound to the
LAN IPv4 address on port **8080**. Its ACL grants only the listed statistics
methods; there are no write, exec, file, UCI, reboot, or network-control grants.
The account does not need a Unix shell user. Existing LuCI and vendor listener
configuration is retained; restarting rpcd/uhttpd briefly interrupts their
current sessions. Rerunning updates the same named sections.

Set `DASHBOARD_LAN_IP` and `DASHBOARD_HTTP_PORT` before running the script if
needed. Choose an unused port. It creates no firewall rules. This example uses
unencrypted HTTP on a trusted LAN: the password and session token travel over
that connection. Keep the listener on the LAN; use a dedicated dashboard
password. HTTPS is not implemented, and an `https://` URL is rejected.

The empty web root is `/etc/ubus-dashboard-empty`. To remove this setup,
delete `rpcd.ubus_dashboard`, `uhttpd.ubus_dashboard`, and
`/usr/share/rpcd/acl.d/ubus-dashboard.json`, commit both UCI packages, and restart
the two services.

Protocol references: [uhttpd ubus bridge](https://github.com/openwrt/uhttpd/blob/master/ubus.c),
[rpcd sessions and ACLs](https://github.com/openwrt/rpcd/blob/master/session.c),
[iwinfo methods](https://github.com/openwrt/rpcd/blob/master/iwinfo.c),
[system fields](https://github.com/openwrt/procd/blob/master/system.c), and
[netifd counters](https://github.com/openwrt/netifd/blob/master/system-linux.c).

## Build and flash

This directory is a separate Cargo workspace using the same esp-hal 1.1.1 /
esp-radio 0.18 generation as `razem-tag`. Its lockfile is committed. The stable
toolchain has a prebuilt `riscv32imc-unknown-none-elf` target; no nightly or
`build-std` is needed for this smaller firmware.

```sh
cd examples/esp32c3-router-display
cp config.example.sh config.sh
# Edit config.sh, including UBUS_URL and optional WIFI_DEVICE, then:
. ./config.sh
cargo build --locked --release --features firmware
# With the board attached and espflash installed:
cargo run --locked --release --features firmware
```

For the router script's default listener on a typical GL.iNet LAN,
`UBUS_URL=http://192.168.8.1:8080/ubus`. `config.sh` is ignored by Git. Credentials
are compiled into the firmware, so keep firmware artifacts private. The flash
configuration uses the source gadget's DIO/80 MHz setting; its QIO mode does
not boot reliably. This example does not access or erase Matter/NVS state, but
flashing replaces the application.

The first counter sample establishes a baseline. Rates appear after the next
successful sample and use measured elapsed time and 64-bit counters. Router
reboots, device changes, decreasing counters, and request failures reset the
baseline and add gaps to the graph. A Wi-Fi or router restart triggers
reconnection; an expired rpcd session triggers one login and retry. Persistent
permission denial remains visible; each call retries authentication at most
once.

## Checks without hardware

From the repository root:

```sh
cargo test --locked --manifest-path examples/esp32c3-router-display/Cargo.toml --lib
cargo clippy --locked --manifest-path examples/esp32c3-router-display/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path examples/esp32c3-router-display/Cargo.toml -- --check
```

From this directory, explicitly add `--target x86_64-unknown-linux-gnu` for host
tests, since its `.cargo/config.toml` selects the bare-metal target. Portable
tests exercise rpcd authentication/retries, malformed replies, field decoding,
load/signal units, elapsed-time rates, counter resets, and missing data. A
successful cross-build validates the hardware APIs and links a real C3 image;
actual screen/touch/radio behavior still needs a board and router.

To preview all five pages with illustrative data using the actual tile renderer:

```sh
cargo run --locked --manifest-path examples/esp32c3-router-display/Cargo.toml --example preview -- /tmp/router-preview.ppm
```

Open the PPM file in an image viewer. This runs on the host, with `std` used
only by the preview program to write the file; the renderer and firmware stay
`no_std`.
