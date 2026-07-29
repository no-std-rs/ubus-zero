#!/usr/bin/env bash
#
# Build the ubusd-builder Docker image and extract `ubusd`, `ubus`, `uci`,
# `rpcd`, and their shared-library deps into `.dep/bin/`.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/.dep/bin"

mkdir -p "$OUT"

if ! docker buildx version >/dev/null 2>&1; then
    echo "error: docker buildx not available — install Docker 20.10+" >&2
    exit 1
fi

# UBUSD_PLATFORM wins (covers exotic hosts and the Apple-Silicon +
# stock cross-rs case where the runner arch differs from the host).
# Otherwise default to the host arch.
if [[ -n "${UBUSD_PLATFORM:-}" ]]; then
    platform="$UBUSD_PLATFORM"
else
    case "$(uname -m)" in
        arm64|aarch64) platform="linux/arm64" ;;
        x86_64|amd64)  platform="linux/amd64" ;;
        *) echo "error: unsupported host arch $(uname -m); set UBUSD_PLATFORM" >&2; exit 1 ;;
    esac
fi

# UBUSD_BUILD_ARGS lets the drift job override the pinned revs, e.g.
# UBUSD_BUILD_ARGS="--build-arg LIBUBOX_REV=master --build-arg UBUS_REV=master".
# shellcheck disable=SC2086
docker buildx build \
    --platform "$platform" \
    --target export \
    --output "type=local,dest=$OUT" \
    ${UBUSD_BUILD_ARGS:-} \
    -f "$ROOT/tools/ubusd-builder/Dockerfile" \
    "$ROOT/tools/ubusd-builder"

# `--output type=local` drops the executable bit when re-exporting
# from the scratch stage; restore it.
chmod +x "$OUT/ubusd" "$OUT/ubus" "$OUT/uci" "$OUT/rpcd"

echo
echo "Extracted to $OUT:"
ls -la "$OUT"
echo
echo "Run 'cargo test -- --ignored' to exercise the broker matrix."
