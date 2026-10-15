#!/usr/bin/env bash
set -euo pipefail

if [[ -x .dep/bin/ubusd && -f .dep/bin/ubusd ]]; then
    exec python3 tools/ubus-compat-check.py
fi

# Runs in parallel (default thread count): the harness is parallel-safe —
# per-invocation unique sockets, per-child `Command::env` LD_PRELOAD, no
# process-global `set_var`, shims compiled read-only in build.rs — and the
# readiness barriers + contention-tolerant deadlines (docs §P7) keep it
# deterministic under the resulting contention.
exec tools/cargo.sh test --workspace --all-features --no-fail-fast
