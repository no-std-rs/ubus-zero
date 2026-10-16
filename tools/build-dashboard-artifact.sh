#!/bin/sh
# A public factory image: never inherit a developer's compile-time credentials.
set -eu
repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${1:-"$repo_dir/dist/firmware"}
mkdir -p "$output_dir"
output_dir=$(CDPATH='' cd -- "$output_dir" && pwd)
example_dir="$repo_dir/examples/esp32c3-router-display"
cd "$example_dir"
env -u WIFI_SSID -u WIFI_PASSWORD -u UBUS_PASSWORD -u UBUS_URL -u UBUS_USERNAME \
    -u WAN_INTERFACE -u WIFI_DEVICE cargo build --locked --release --features firmware
espflash save-image --skip-update-check --chip esp32c3 --merge --skip-padding \
    --flash-mode dio --flash-freq 80mhz --flash-size 4mb \
    --partition-table partitions.csv \
    target/riscv32imc-unknown-none-elf/release/router-display "$output_dir/router-display.bin"
cd "$repo_dir"
python3 tools/package-dashboard.py "$output_dir"
