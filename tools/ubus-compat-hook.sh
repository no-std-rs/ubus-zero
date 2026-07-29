#!/usr/bin/env bash
set -euo pipefail

if [[ -x .dep/bin/ubusd && -f .dep/bin/ubusd ]]; then
    export UBUS_ZERO_COMPAT=require
fi

# Runs in parallel (default thread count): the harness is parallel-safe —
# per-invocation unique sockets, per-child `Command::env` LD_PRELOAD, no
# process-global `set_var`, shims compiled read-only in build.rs — and the
# readiness barriers + contention-tolerant deadlines (docs §P7) keep it
# deterministic under the resulting contention.
exec tools/cargo.sh test --no-default-features --features testing --test ubusd_handshake
