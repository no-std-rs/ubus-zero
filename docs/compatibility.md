# Compatibility testing

The compatibility rule is simple: on supported protocol paths, the Rust `ubusd`
matches the pinned OpenWrt `ubusd` reference behavior byte for byte. A detected
divergence is fixed in the Rust implementation or in the shared canonicalizer;
individual tests do not get normalization escape hatches.

The current reference revisions are pinned in
[`tools/ubusd-builder/Dockerfile`](../tools/ubusd-builder/Dockerfile):

- libubox `17f527f`
- ubus `24864e7`

Revision bumps are deliberate changes. They require rebuilding the fixtures,
recording the corpus serially, reviewing every golden diff, and running the
strict suite.

Unresolvable ACL identity names fail closed, following upstream's later
`008655958eb46de8630eb30c7bb09c3530d07dac` correction. They never inherit UID or
GID zero. The pinned C reference predates that fix; dedicated Rust regressions
cover this invalid-configuration case without normalizing pinned transcripts.

Supplementary-group parsing also reads complete proc records before applying
the 64-group limit. The pinned reference's 511-byte chunk can truncate a
numeric GID into another valid ID; dedicated Rust regressions prevent that
false grant and retain legitimate groups beyond the old buffer boundary.

## Test layers

| Layer | Purpose | Runs without Docker artifacts |
| --- | --- | :---: |
| Core unit and round-trip tests | framing, codecs, client, server, transport contracts | yes |
| Rust `ubusd` smoke tests | broker routing, ACL, lifecycle, limits, malformed input | yes |
| Golden replay | compare the Rust broker with recorded upstream transcripts | yes |
| Live broker matrix | run scenarios against upstream and Rust brokers | no |
| Canonical parity | normalize broker-minted IDs, then compare complete transcripts | no |
| Real CLI and libubus roles | exercise behavior through OpenWrt consumers/providers | no |
| rpcd sessions, UCI, and file plugin | authentication, authorization, private deltas, committed state, file operations | no |
| Fault profiles | fragmentation, partial I/O, and EINTR handling | no |
| Differential fuzzing | deterministic robustness and regression corpus | partly |

Checked-in goldens keep upstream behavior testable in an ordinary clone.
Whenever the live reference binaries are present, the same tests also verify
that the goldens still match those pinned binaries.

## Strict and lenient modes

Reference-backed tests are lenient by default: a genuinely absent upstream
fixture prints a visible `SKIP` and leaves local development usable.

Set strict mode in CI or before changing protocol behavior:

```sh
python3 tools/ubus-compat-check.py
```

In strict mode, any missing reference dependency is a failure. An inconsistent
fixture installation—for example, shared libraries present but unusable—is
always a failure even in lenient mode.

The strict runner builds the entire workspace once, obtains test executables
and the fresh Rust broker path from Cargo's JSON artifact output, and runs
independent test binaries concurrently. It runs every suite and the doctests
even if an earlier suite fails. This also avoids accidentally selecting an old
`target/debug/ubusd` when `CARGO_TARGET_DIR` points elsewhere.

By default, up to four test binaries run together, sharing a budget of roughly
twice the available CPU count in libtest threads. Tune concurrency explicitly
when stressing isolation or running on a constrained machine:

```sh
UBUS_ZERO_TEST_JOBS=4 RUST_TEST_THREADS=32 python3 tools/ubus-compat-check.py
```

`UBUS_ZERO_TEST_TIMEOUT` sets the per-executable deadline in seconds (default
300). Native Linux executes the binaries directly; other hosts retain the
configured `tools/cargo.sh` target runner.

Each native suite runs in a private Linux session. On completion, timeout, or
cancellation, the runner stops its live process groups, including shell
handlers that create their own group and children whose parents have exited.
Session-isolation regressions verify this cleanup leaves neighboring suites
running.

## Building the reference fixtures

The builder needs Docker Buildx and writes ignored artifacts under `.dep/bin/`:

```sh
tools/ubusd-builder/build.sh
```

It exports pinned builds of `ubusd`, the `ubus` CLI, `uci`, `rpcd`, their
headers, and the shared libraries needed by the narrow test FFI.

The harness resolves overrides before local artifacts:

- `UBUS_ZERO_UBUSD`
- `UBUS_ZERO_UBUSD_RS`
- `UBUS_ZERO_CLI`
- `UBUS_ZERO_UCI`
- `UBUS_ZERO_RPCD`
- `UBUS_ZERO_LIBUBOX_DIR`

These are development controls, not public library configuration.

## Canonical transcripts

A `WireTap` sits between each test role and the broker. It decodes both stream
directions, records global arrival order, and hands the result to one shared
canonicalizer.

Broker-minted object, peer, and type identifiers differ naturally between
runs. The canonicalizer maps them to first-appearance placeholders and rewrites
every known ID-bearing location. Everything else stays exact: headers,
attribute ordering, padding, lengths, method names, payloads, and status codes.
Scenarios without minted IDs compare as raw bytes.

Goldens are human-reviewable, line-oriented canonical transcripts containing:

- the scenario name;
- pinned ubus and libubox revisions;
- connection and direction;
- exact frame bytes.

Record mode is intentionally serial because canonical identifier assignment is
arrival-order-sensitive:

```sh
tools/ubus-record.sh
```

Never hand-edit a golden to make a failure disappear. Either regenerate it
because the pinned upstream revision intentionally changed, or fix the
implementation.

## Fault injection

The testkit builds small Linux `LD_PRELOAD` shims for peer credentials, name
service results, filesystem metadata, path redirection, and I/O faults. Each
broker process receives its own environment; the test process does not mutate
global environment state.

ACL tests can assign distinct users, primary and supplementary groups, and
ACL-file ownership/modes to each broker without changing host accounts or
requiring `chown`. Supplementary-group fixtures cover both pinned `/proc`
reads and newer `SO_PEERGROUPS` queries, including kernel buffer-length errors.
Every rpcd instance gets private ACL, plugin, configuration,
delta, override, and runtime directories. The redirector covers libc stdio,
temporary-file creation, chmod/rename/unlink, and plugin loading, so UCI commits
and rpcd sessions can run concurrently without sharing host configuration.
The redirector itself has an executable regression probe for those operations.

Two child-local rpcd adjustments isolate upstream scheduling from host load.
After a successful real `fsync` of a file under the private fixture root, the
redirector omits the next machine-wide `sync`; file contents, permissions,
`fsync` failures, and writes outside that root remain observable. The exec
shim defers pipe EOF until rpcd's normal child callback records the actual
exit status. Native probes verify these boundaries without synthesizing
replies or exit codes.

FD tests use direct sockets because SCM_RIGHTS cannot pass through the wire
tap. They cover fragmented headers and bodies, descriptor placement at framing
boundaries, pipelined calls, coalesced replies, error-path disposal, disconnects,
and concurrent independent routes. The FD peer retains incomplete frames and
their descriptor across timeouts and consumes each complete frame exactly once.
Pipe readiness and protocol barriers replace timing sleeps for cleanup checks.
Large Rust-only queueing regressions also retain coverage where the pinned C
daemon has known FD lifetime or edge-triggered receive limitations.

The gated profiles are:

```sh
tools/ubus-fault-check.sh
```

- `fragment=3`: fragments client-to-broker streams symmetrically.
- `eintr_every=3`: injects EINTR into the upstream broker's write path.

Both profiles run the complete strict workspace concurrently. Fragmentation
applies to all tapped live and golden cases, including ACLs; direct FD cases
exercise their own explicit fragmentation and ancillary-data boundaries. CLI,
rpcd, lifecycle, and direct FD cases also run under the broker write-fault
profile.

`max_write` and receive-side EINTR remain diagnostic profiles because they
expose known reference-broker edge behavior rather than a portable contract.

## Differential fuzzing

The deterministic fuzzer generates structurally valid frames and controlled
corruptions from literal or environment-provided seeds. Its always-on contract
is robustness: neither broker may crash, hang, or become unable to answer a
fresh ping.

Arbitrary malformed input is outside the byte-parity contract. the Rust `ubusd` may
strict-close where upstream returns an error. Accepted strict-close cases and
fixed bugs live in `crates/testkit/corpus/`; explored long-tail status
differences do not.

Useful controls:

```sh
UBUS_ZERO_COMPAT=require UBUS_ZERO_FUZZ_SEEDS=1024 \
  cargo test -p ubus-testkit --test differential_fuzz -- \
  --ignored differential_fuzz_parity_explore --nocapture

UBUS_ZERO_FUZZ_TRIAGE_SEEDS=6,11,46 \
  cargo test -p ubus-testkit --test differential_fuzz -- \
  --ignored differential_fuzz_triage_seeds --nocapture
```

Every genuine bug discovered by fuzzing must land with a named regression.

## CI and upstream drift

[`ci.yml`](../.github/workflows/ci.yml) runs formatting, Clippy with warnings
denied, the entire stable workspace, packaging, and the strict pinned-reference
suite. Every third-party action is pinned to a full commit SHA.

The compatibility workflow also runs the complete strict workspace weekly
against upstream `master` as a non-blocking drift signal. It continues through
all test executables after failures so ACL, FD, lifecycle, rpcd, CLI, and fuzz
results remain visible even when the handshake corpus has already diverged.
A red drift job is not fixed by moving the pinned revision immediately; it
starts a deliberate compatibility review. Passing the pinned suite establishes
the pinned contract, while a failing drift job means current upstream behavior
still needs review.

## Change checklist

For protocol, broker, canonicalizer, or golden changes:

1. Add or update the smallest focused unit/smoke test.
2. Build the pinned fixtures.
3. Run the strict workspace suite.
4. Run both gated fault profiles.
5. If the upstream pin changed, regenerate goldens serially and review the wire
   diff.
6. Confirm an ordinary no-`.dep` clone still passes through unit tests and
   golden replay rather than vacuous skips.
