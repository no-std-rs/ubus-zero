#!/usr/bin/env bash
# Degraded-I/O torture for the compat corpus.
#
# Gated profiles (the full workspace must stay green under
# each — degraded I/O must be invisible above the stream):
#   * fragment=N   — tap-side client→broker fragmentation. Hits BOTH
#                    brokers symmetrically (it sits on the socket stream,
#                    not inside a broker), so framing must be invisible.
#   * eintr_every=K — libc-shim: fail every K-th broker write with EINTR.
#                    Reaches upstream only (ubusd-rs uses raw rustix
#                    syscalls), so it torture-tests upstream's EINTR
#                    retry; rs's own retry is covered by its smoke tests.
#                    K=3 leaves 2/3 of writes succeeding, so the broker
#                    always makes forward progress.
#
# NOT gated — `max_write=N` (cap each broker write to N bytes), a
# manual-only probe. Findings from 2026-07-11:
#   * `cap_iov` (fault.c) is byte-correct: at N >= the largest frame the
#     full parity+golden corpus is green, so the shim never loses or
#     reorders bytes (the old "lossy HELLO" note was wrong).
#   * N below the 8-byte frame header stalls upstream's *own* HELLO — an
#     upstream write-path floor (it never re-enters its send loop for a
#     sub-header partial), not a shim defect.
#   * At intermediate N the fan-out scenarios truncate: capping writes
#     slows the broker's teardown frames (active=0 / object.remove) so
#     much that a peer's socket closes before the broker flushes them,
#     and the tap captures a partial. That is an artifact of extreme
#     write-slowing racing connection teardown — neither a shim nor a
#     broker bug — so max_write is a hand-run probe, not a gated profile.
#     Run it manually, e.g.:
#       UBUS_ZERO_FAULT=max_write=64 UBUS_ZERO_COMPAT=require \
#         tools/cargo.sh test --no-default-features --features testing \
#         --test ubusd_handshake -- --test-threads=1 \
#         add_object_replies_data_objid_then_status_ok
#
# NOT gated — `rx_eintr_every=K` (fail every K-th read/recv/recvmsg on
# an accepted-client fd with EINTR). Findings from 2026-07-13:
#   * Upstream's client_cb runs under ULOOP_EDGE_TRIGGER and answers a
#     negative recvmsg/read with `goto out`. An injected EINTR that eats
#     the edge for a client's FINAL pending write therefore wedges that
#     connection until new bytes or EOF arrive — at K=3 the corpus fails
#     en masse (hello_handshake upstream arm included). That mirrors a
#     real (if unlikely) upstream signal-timing hazard, not a shim bug,
#     and rs is unaffected (raw rustix reads bypass libc interposers).
#     Manual-only probe; keep it out of the gate.
set -euo pipefail

run_profile() {
  local profile="$1"; shift
  echo "== UBUS_ZERO_FAULT=${profile} :: strict workspace"
  exec env UBUS_ZERO_FAULT="${profile}" UBUS_ZERO_COMPAT=require \
    python3 tools/ubus-compat-check.py
}

# Separate child environments keep the profiles independent. Capture each
# profile's output together while exercising both profiles concurrently.
logs="$(mktemp -d)"
fragment_pid=
eintr_pid=
cleanup() {
  for pid in "$fragment_pid" "$eintr_pid"; do
    if [[ -n "$pid" ]]; then
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  for pid in "$fragment_pid" "$eintr_pid"; do
    if [[ -n "$pid" ]]; then
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$logs"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
run_profile "fragment=3" >"$logs/fragment" 2>&1 &
fragment_pid=$!
run_profile "eintr_every=3" >"$logs/eintr" 2>&1 &
eintr_pid=$!
status=0
wait "$fragment_pid" || status=1
fragment_pid=
wait "$eintr_pid" || status=1
eintr_pid=
cat "$logs/fragment"
cat "$logs/eintr"
exit "$status"
