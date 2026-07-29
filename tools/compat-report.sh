#!/usr/bin/env bash
#
# Human decision aid: render the ubus compat corpus as a
# scenario × arm (upstream / rs / parity / golden) matrix with
# pass / fail / SKIP as first-class outcomes. This is NOT a gate — the
# pre-commit hook and CI stay plain pass/fail (see tools/ubus-compat-hook.sh
# and .github/workflows/ci.yml). Its job is to answer "did the
# coverage actually run, or did it silently skip?" at a glance.
#
# Runs in LENIENT mode on purpose so absent-fixture skips render as SKIP
# cells instead of aborting; pass UBUS_ZERO_COMPAT=require to turn
# skips into failures (matching CI). A missing `.dep/bin/ubusd` therefore
# shows a wall of SKIPs rather than a wall of green.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

raw="$(mktemp)"
trap 'rm -f "$raw"' EXIT

echo "Running ubus compat corpus (lenient; this spawns brokers)…" >&2
# The matrix + CLI + ACL scenarios. `|| true`: a red arm must still be
# reported, not abort the run. Deliberately single-threaded even though
# the suites are parallel-safe (the pre-commit hook and CI run them
# parallel): this report PARSES per-test `--nocapture` stdout, and
# parallel tests interleave their output line-by-line, which would corrupt
# the scenario × arm classification below. Determinism of the parse, not
# broker isolation, is why this stays serial.
for t in ubusd_handshake ubusd_fd_passing ubus_cli_cross ubusd_acl \
         ubusd_lifecycle ubusd_concurrency; do
    tools/cargo.sh test --no-default-features --features testing --test "$t" -- \
        --test-threads=1 --nocapture 2>&1 || true
done >>"$raw"
# The differential fuzzer lives in ubus-testkit.
{ tools/cargo.sh test -p ubus-testkit --test differential_fuzz -- --nocapture 2>&1 || true; } >>"$raw"

awk '
# Collect every SKIP line so an "ok" case that actually skipped is
# reclassified below.
/^SKIP / {
    line = substr($0, 6)
    skips[++nskip] = line
    next
}
# Matrix / CLI cases: "test <scenario>::case_<n>_<arm> ... <outcome>".
match($0, /^test (.+)::case_[0-9]+_([a-z]+) \.\.\. (ok|FAILED|ignored)/, m) {
    scen = m[1]; arm = m[2]; res = toupper(m[3])
    seen_scen[scen] = 1
    arms[arm] = 1
    cell[scen, arm] = (res == "OK" ? "pass" : (res == "FAILED" ? "FAIL" : "ign"))
    next
}
# Arm-less cases (fuzzer robustness / corpus, testkit unit tests).
match($0, /^test ([A-Za-z0-9_]+) \.\.\. (ok|FAILED|ignored)/, m) {
    other[m[1]] = toupper(m[2])
    next
}
END {
    # An "ok" cell whose scenario+arm appears in a SKIP line is a skip.
    for (i = 1; i <= nskip; i++) {
        for (key in cell) {
            split(key, kv, SUBSEP); s = kv[1]; a = kv[2]
            if (cell[s, a] == "pass" && index(skips[i], s) && index(skips[i], a)) {
                cell[s, a] = "SKIP"
            }
        }
    }

    order = "upstream rs parity golden"
    n = split(order, col, " ")
    printf "\n%-62s", "scenario"
    for (j = 1; j <= n; j++) printf "%-10s", col[j]
    printf "\n%s\n", "----------------------------------------------------------------------------------------------"

    pass = 0; fail = 0; skip = 0
    ncols = asorti(seen_scen, sorted)
    for (r = 1; r <= ncols; r++) {
        s = sorted[r]
        printf "%-62s", s
        for (j = 1; j <= n; j++) {
            a = col[j]; v = cell[s, a]
            if (v == "") v = "-"
            printf "%-10s", v
            if (v == "pass") pass++
            else if (v == "FAIL") fail++
            else if (v == "SKIP") skip++
        }
        printf "\n"
    }

    printf "\nnon-matrix tests (robustness / fuzzer / unit):\n"
    cnt = asorti(other, os)
    for (r = 1; r <= cnt; r++) printf "  %-52s %s\n", os[r], other[os[r]]

    printf "\nmatrix totals: pass=%d fail=%d skip=%d\n", pass, fail, skip
    if (nskip > 0) {
        printf "\nSKIP details (%d):\n", nskip
        for (i = 1; i <= nskip; i++) printf "  %s\n", skips[i]
    }
}
' "$raw"
