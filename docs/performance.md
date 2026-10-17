# Broker performance comparisons

The benchmark runs identical, verified protocol operations against the pinned
OpenWrt C `ubusd` and the Rust broker. It measures end-to-end latency and
throughput over local Unix sockets. It does not measure isolated broker CPU
time, memory use, or OpenWrt router hardware.

## Run

On Linux, with the pinned Rust toolchain, a C compiler, and Docker Buildx:

```sh
tools/ubusd-builder/build.sh
python3 tools/ubus-benchmark.py
```

The runner builds both Rust executables in release mode and obtains their
actual paths from Cargo's JSON output. It honors `CARGO_TARGET_DIR` and never
falls back to an existing debug binary. The default C reference comes from
`.dep/bin/ubusd`; a missing or unusable broker is a failure, not a skip.

Open `dist/benchmarks/report.html` for the comparison. The directory also
contains `performance.json` (all raw latency samples and provenance),
`summary.csv`, and `report.md`. The HTML is self-contained and needs no
external scripts, fonts or services.

```sh
python3 tools/ubus-benchmark.py --iterations 5000 --samples 6 --warmup 250
python3 tools/ubus-benchmark.py --smoke    # harness validation, not a performance result
```

`--iterations` counts operations **per client per sample**. `--upstream`
selects another reference executable; `--output` selects the report directory.

## Workloads

| Operation | Clients | Verification |
| --- | ---: | --- |
| Connect, HELLO, close | 1 | Valid nonzero broker-assigned identity |
| Ping on a persistent connection | 1 | Successful ubus reply |
| Lookup of a registered object | 1 | Exact registered object ID |
| Echo, 64 / 4,096 / 32,768-byte string | 1 | Exact returned blobmsg bytes, including client ID and per-request sequence |
| Echo, 4,096-byte string | 4 | Independent identities and sequences; every reply verified |

Payload sizes describe the string contents, excluding blobmsg and protocol
overhead. Echo and lookup use the same Rust object provider on both brokers.
The provider waits for socket readiness between requests; there is no fixed
sleep added to each reply. The concurrent workload has one provider serving
four clients, so its scheduling can also limit throughput.

Each sample gets a fresh broker. Registration, connection setup and warm-up
finish before measurement. Connect is the exception: opening the socket,
validating HELLO and closing the connection are the operation being measured.
Each client issues one request at a time; this is a closed-loop workload,
not a measurement of latency under an independently fixed arrival rate.

## Interpret the report

- p50, p95 and p99 are nearest-rank percentiles over all measured operations
  for that workload and broker. Warm-up operations are excluded.
- Throughput is the median of the repeated samples' operation rates. Each
  rate divides **total completed operations by concurrent wall time**;
  summing individual client latencies would understate concurrent throughput.
  Wall time includes the start barrier, thread joins and connection cleanup.
- Latency includes client serialization, transport, broker routing, provider
  execution, reply parsing and validation. It is not broker processing time.
- Broker order alternates across samples and workloads. Both use direct
  sockets and the compatibility harness's same child-local root-credential
  shim. This permits named registration without host ACL changes; it does
  not benchmark ACL evaluation. Wire taps and I/O faults are excluded.
  Ambient fault, preload and test-identity overrides cause a hard failure.
- The reference builder uses CMake Release. Rust uses this repository's
  release profile, including `opt-level = "z"` and LTO. These are the existing
  build settings, not identical compiler optimization policies.

The JSON records the source commit and dirty state, Rust compiler, build
profiles, executable and reference-library SHA-256s, reference recipe pins,
CPU, affinity, kernel and starting system load. Recipe pins describe the
checked-in builder; a user-supplied reference binary is identified by its
hash, not independently authenticated as that revision.

Run on a quiet host and keep machine, power settings, affinity, reference
binaries, toolchain and build profiles fixed when comparing changes. Shared
CI hosts are useful for checking the harness, but their timings are not a
portable performance claim. CPU scheduling and client/provider overhead can
hide broker differences; repeat surprising results before optimizing.

## CI

The **Broker performance** workflow validates all workloads in a short PR
smoke run. Main-branch changes, weekly runs and manual dispatches use the full
defaults: 1,000 operations per client, five samples and 100 warm-up operations.
The job uploads the HTML, CSV, Markdown and raw JSON as `broker-performance`
and adds its table to the Actions summary.

Wrong replies, missing fixtures and broken runs fail CI. Latency and
throughput have no pass/fail thresholds. The existing strict compatibility
and fault-profile gates continue to establish protocol correctness.
