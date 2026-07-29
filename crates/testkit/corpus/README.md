# Differential-fuzz regression corpus

Each `*.bin` here is the raw byte blob for one **single-connection** fuzz
case. The bytes are sent verbatim to a fresh connection of each broker
(after draining the unsolicited HELLO); `differential_fuzz.rs::
differential_fuzz_corpus` replays every file under the **tolerant response
oracle** (`compare`): `ubusd-rs` may be *stricter* than upstream — it may
close the peer where upstream returns an error status (its documented
malformed-input close path) — but any other response difference, or rs being
*more lenient*, fails. Never loosen the oracle to make a case pass.

Two kinds of case earn a file:

1. **Fixed-bug regressions.** A divergence triaged as a genuine `ubusd-rs`
   bug on an *exercised* protocol path: fix rs per doctrine (match upstream),
   land the fix with the case so it goes green, and it guards the fix.
2. **Strict-close drift guards.** A malformed input where rs already
   strict-closes and upstream errors — the tolerant oracle passes it today.
   The file pins that relationship so a future `.dep/bin` pin bump (or an rs
   change) that alters either side is caught. The two files here
   (`strict-close-*.bin`) are of this kind, mined by
   `differential_fuzz_strict_close_scan` (seeds 66 and 981).

Byte-for-byte response parity over **arbitrary** input is a non-goal — rs
speaks ubus-zero's supported protocol surface, not full libubus, and
diverges harmlessly on the long tail. The exercised paths are pinned by the
`tests/ubusd_handshake.rs` matrix, not here.

## Triage runbook

1. **Survey** — sweep seeds under the tolerant oracle, grouped by divergence
   class (non-aborting; always passes, it is a tool not a gate):

   ```
   UBUS_ZERO_COMPAT=require UBUS_ZERO_FUZZ_SEEDS=1024 \
     cargo test -p ubus-testkit --test differential_fuzz -- \
     --ignored differential_fuzz_parity_explore --nocapture
   ```

   Each line is `seed N: <class>`; the tail is a `<count>× <class>
   (exemplar seed S)` summary.

2. **Triage** — deep-dive the exemplar seeds: prints each seed's exact
   per-connection input hex (the corpus payload for single-connection cases)
   plus the full divergence detail:

   ```
   UBUS_ZERO_FUZZ_TRIAGE_SEEDS=6,11,46 \
     cargo test -p ubus-testkit --test differential_fuzz -- \
     --ignored differential_fuzz_triage_seeds --nocapture
   ```

3. **Classify** each class:
   - *genuine rs bug on an exercised path* → fix rs, add its `conn0` bytes
     as a fixed-bug regression file (kind 1). After T1's error-path parity
     work and M1's MONITOR support, the exercised paths are matrixed and no
     such class currently survives the survey.
   - *rs strict-closes where upstream errors* → mine with
     `differential_fuzz_strict_close_scan` and capture as a drift guard
     (kind 2).
   - *accepted scope difference* (rs answers a different error status, or
     responds to an arbitrary/corrupt frame upstream ignores) → leave it in
     the survey. This is the expected long tail; do **not** fix rs and do
     **not** force it into the corpus (the tolerant oracle rejects it by
     design).

### Current survey baseline (2026-07-13, 1024 seeds, 12-type grammar)

90/1024 divergent, entirely in accepted long-tail classes — status-code
differences on corrupt/arbitrary frames (`up=Status(6) rs=Status(4)`,
`up=Status(4) rs=Status(2)`, …) and framing cascades on multi-frame corrupt
input (`up=missing rs=Status(1)`, `up=Data rs=Data` id-numbering artifacts on
cross-referencing cases). No exercised-path rs bug survives — those were
closed by the error-path parity (`ubus: error-path status parity …`) and
MONITOR work. Adding a `.bin` here for one of these long-tail classes would
break `differential_fuzz_corpus`, which is the point: the corpus is for
fixed bugs and strict-close guards, not for pinning intentional divergence.
