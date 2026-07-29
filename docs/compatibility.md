# Compatibility testing

The compatibility rule is simple: on supported protocol paths, `ubusd-rs`
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

## Test layers

| Layer | Purpose | Runs without Docker artifacts |
| --- | --- | :---: |
| Core unit and round-trip tests | framing, codecs, client, server, transport contracts | yes |
| `ubusd-rs` smoke tests | broker routing, ACL, lifecycle, limits, malformed input | yes |
| Golden replay | compare the Rust broker with recorded upstream transcripts | yes |
| Live broker matrix | run scenarios against upstream and Rust brokers | no |
| Canonical parity | normalize broker-minted IDs, then compare complete transcripts | no |
| Real CLI and libubus roles | exercise behavior through OpenWrt consumers/providers | no |
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
UBUS_ZERO_COMPAT=require cargo test --workspace --all-features
```

In strict mode, any missing reference dependency is a failure. An inconsistent
fixture installation—for example, shared libraries present but unusable—is
always a failure even in lenient mode.

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

The gated profiles are:

```sh
tools/ubus-fault-check.sh
```

- `fragment=3`: fragments client-to-broker streams symmetrically.
- `eintr_every=3`: injects EINTR into the upstream broker's write path.

`max_write` and receive-side EINTR remain diagnostic profiles because they
expose known reference-broker edge behavior rather than a portable contract.

## Differential fuzzing

The deterministic fuzzer generates structurally valid frames and controlled
corruptions from literal or environment-provided seeds. Its always-on contract
is robustness: neither broker may crash, hang, or become unable to answer a
fresh ping.

Arbitrary malformed input is outside the byte-parity contract. `ubusd-rs` may
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

The compatibility workflow also runs weekly against upstream `master` as a
non-blocking drift signal. A red drift job is not fixed by moving the pinned
revision immediately; it starts a deliberate compatibility review.

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
