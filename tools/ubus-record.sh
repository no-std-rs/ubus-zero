#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKERFILE="$ROOT/tools/ubusd-builder/Dockerfile"

libubox_rev="$(grep -E '^ARG LIBUBOX_REV=' "$DOCKERFILE" | tail -n1 | cut -d= -f2-)"
ubus_rev="$(grep -E '^ARG UBUS_REV=' "$DOCKERFILE" | tail -n1 | cut -d= -f2-)"

export UBUS_ZERO_GOLDEN=record
export UBUS_ZERO_REVS="$ubus_rev $libubox_rev"
export UBUS_ZERO_COMPAT=require

# Deliberately single-threaded even though the suites are parallel-safe:
# this is the golden byte-capture path, where determinism trumps speed.
# The canonical id numbering derives from cross-connection arrival order
# (docs §P2), so capturing goldens under a fixed, serial schedule keeps
# the recorded transcripts stable run-to-run.
for suite in ubusd_handshake ubusd_acl; do
    "$ROOT/tools/cargo.sh" test \
        --no-default-features \
        --features testing \
        --test "$suite" \
        -- \
        --test-threads=1 \
        case_1_upstream
done
