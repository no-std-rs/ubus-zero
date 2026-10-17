#!/usr/bin/env python3
"""Verified C/Rust broker comparisons; timings are observations, never CI thresholds."""

import argparse
import csv
import datetime
import hashlib
import html
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("benchmark_processes", ROOT / "tools/ubus-compat-check.py")
processes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(processes)
CASES = [
    ("connect", 1, 0), ("ping", 1, 0), ("lookup", 1, 0),
    ("echo", 1, 64), ("echo", 1, 4096), ("echo", 1, 32768), ("echo", 4, 4096),
]


def command(args, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, text=True, **kwargs)


def digest(path):
    with path.open("rb") as handle:
        return hashlib.file_digest(handle, "sha256").hexdigest()


def build():
    result = processes.run_cargo(["cargo", "build", "--release", "--locked", "-p", "ubusd",
                                 "-p", "ubus-testkit", "--bins", "--message-format=json"],
                                os.environ.copy(), capture_stdout=True)
    result.check_returncode()
    binaries = {}
    profiles = {}
    for line in result.stdout.splitlines():
        artifact = json.loads(line)
        if artifact.get("reason") == "compiler-artifact" and artifact.get("executable"):
            name = artifact["target"]["name"]
            binaries[name] = Path(artifact["executable"]).resolve()
            profiles[name] = artifact["profile"]
    if not {"ubusd", "ubus-bench"} <= binaries.keys():
        raise ValueError("Cargo did not report both benchmark executables")
    return binaries, profiles


def percentile(values, percent):
    """Nearest-rank percentile, with no interpolation of observed latencies."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * percent / 100) - 1)]


def validate(sample, broker, case, iterations, warmup):
    workload, clients, payload_bytes = case
    expected = {"schema_version": 1, "broker": broker, "workload": workload,
                "clients": clients, "payload_bytes": payload_bytes,
                "iterations": iterations, "warmup": warmup, "operations": iterations * clients}
    for key, value in expected.items():
        if type(sample.get(key)) is not type(value) or sample[key] != value:
            raise ValueError(f"invalid worker result: {key}")
    if type(sample.get("elapsed_ns")) is not int or sample["elapsed_ns"] <= 0:
        raise ValueError("invalid worker elapsed time")
    latencies = sample.get("latency_ns")
    if not isinstance(latencies, list) or len(latencies) != expected["operations"]:
        raise ValueError("incomplete latency samples")
    if any(type(value) is not int or value <= 0 for value in latencies):
        raise ValueError("invalid latency sample")
    if max(latencies) > sample["elapsed_ns"]:
        raise ValueError("latency exceeds measured wall time")


def summarize(samples):
    latency = [value for sample in samples for value in sample["latency_ns"]]
    rates = [sample["operations"] * 1e9 / sample["elapsed_ns"] for sample in samples]
    return {"operations_per_second": statistics.median(rates),
            "min_operations_per_second": min(rates), "max_operations_per_second": max(rates),
            "p50_us": percentile(latency, 50) / 1000,
            "p95_us": percentile(latency, 95) / 1000,
            "p99_us": percentile(latency, 99) / 1000,
            "observations": len(latency)}


def label(case):
    workload, clients, payload_bytes = case
    suffix = f" {payload_bytes} B" if payload_bytes else ""
    return f"{workload}{suffix}, {clients} client{'s' if clients != 1 else ''}"


def run_worker(invocation, environment, timeout=300):
    # Reuse the strict runner's tested session cleanup, including orphaned children.
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as output:
        with processes.managed_process(invocation, environment, stdout=output) as process:
            status = process.wait(timeout=timeout)
        if status:
            raise ValueError(f"benchmark worker exited {status}")
        output.seek(0)
        return json.load(output)


def metadata(binaries, profiles, upstream):
    def identity(path):
        return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest(path)}

    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), platform.machine())
    pins = {}
    for line in (ROOT / "tools/ubusd-builder/Dockerfile").read_text().splitlines():
        if line.startswith(("ARG LIBUBOX_REV=", "ARG UBUS_REV=")):
            key, value = line.removeprefix("ARG ").split("=", 1)
            pins[key] = value
    return {
        "measured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "commit": command(["git", "rev-parse", "HEAD"], stdout=subprocess.PIPE).stdout.strip(),
        "dirty": bool(command(["git", "status", "--porcelain"], stdout=subprocess.PIPE).stdout.strip()),
        "host": {"kernel": platform.release(), "architecture": platform.machine(), "cpu": cpu,
                 "available_cpus": len(os.sched_getaffinity(0)), "load_average": os.getloadavg()},
        "rustc": command(["rustc", "-Vv"], stdout=subprocess.PIPE).stdout.strip(),
        "rust_release_profile": tomllib.loads((ROOT / "Cargo.toml").read_text())["profile"]["release"],
        "cargo_artifact_profiles": profiles,
        "reference_recipe_pins": pins,
        "reference_recipe_sha256": digest(ROOT / "tools/ubusd-builder/Dockerfile"),
        "binaries": {"upstream": identity(upstream), "rust": identity(binaries["ubusd"]),
                     "driver": identity(binaries["ubus-bench"])},
        "reference_shared_libraries": {path.name: identity(path) for path in sorted(upstream.parent.glob("*.so*"))
                                       if path.is_file()},
        "method": "Same Rust client/provider and child-local root-credential shim; direct Unix sockets; "
                  "no wire tap or I/O fault injection. Fresh broker per sample, warmed connections, "
                  "alternating broker order. End-to-end operations include client/provider work and "
                  "reply validation; connect includes socket connect, HELLO and close. "
                  "Throughput uses concurrent wall time including barrier/join overhead. "
                  "Percentiles pool all measured operations; throughput is the median sample rate.",
    }


def write_reports(output, result):
    output.mkdir(parents=True, exist_ok=True)
    rows = []
    for comparison in result["comparisons"]:
        for broker in ("upstream", "rust"):
            rows.append({"workload": comparison["workload"], "clients": comparison["clients"],
                         "payload_bytes": comparison["payload_bytes"], "broker": broker,
                         **comparison[broker]})
    with (output / "summary.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    (output / "performance.json").write_text(json.dumps(result, indent=2) + "\n")
    table = ["| Workload | Broker | ops/s | p50 µs | p95 µs | p99 µs |",
             "| --- | --- | ---: | ---: | ---: | ---: |"]
    body = []
    for comparison in result["comparisons"]:
        name = label((comparison["workload"], comparison["clients"], comparison["payload_bytes"]))
        maximum = max(comparison[broker]["operations_per_second"] for broker in ("upstream", "rust"))
        for broker in ("upstream", "rust"):
            stats = comparison[broker]
            values = [f'{stats[key]:,.2f}' for key in ("operations_per_second", "p50_us", "p95_us", "p99_us")]
            table.append(f"| {name} | {broker} | " + " | ".join(values) + " |")
            body.append(f'<tr><th>{html.escape(name)}</th><td>{broker}</td>'
                        f'<td><meter min="0" max="{maximum}" value="{stats["operations_per_second"]}"></meter> '
                        + values[0] + '</td>' + ''.join(f'<td>{value}</td>' for value in values[1:]) + '</tr>')
    notice = ("Informational end-to-end measurements, not a timing gate or isolated broker CPU measurements. "
              "Shared-host load and the client/provider affect results. PR smoke runs validate the harness, "
              "not performance. See metadata for build profiles, binary hashes and measurement settings.")
    if result["configuration"].get("smoke"):
        notice = "SMOKE RUN: harness verification only; these timings are not performance results. " + notice
    (output / "report.md").write_text("## ubusd performance comparison\n\n" + notice + "\n\n" +
                                     "\n".join(table) + "\n")
    (output / "report.html").write_text('''<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>ubusd performance comparison</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:1100px;margin:3rem auto;padding:0 1rem;color:#17212b}
h1{letter-spacing:-.04em}table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}
th,td{padding:.65rem;text-align:right;border-bottom:1px solid #dce2e7}th:first-child{text-align:left}
meter{width:7rem}pre{white-space:pre-wrap;overflow-wrap:anywhere}small{color:#52616d}
.scroll{overflow-x:auto}details{margin-top:2rem}</style><h1>ubusd performance comparison</h1>'''
        + f'<p>{html.escape(notice)}</p><p><small>Commit {html.escape(result["metadata"]["commit"])} · '
        + f'{result["configuration"]["samples"]} samples per broker/workload · '
        + f'{result["configuration"]["iterations"]} operations per client per sample</small></p>'
        + '<div class="scroll"><table><thead><tr><th>Workload</th><th>Broker</th><th>ops/s</th>'
        + '<th>p50 µs</th><th>p95 µs</th><th>p99 µs</th></tr></thead><tbody>' + ''.join(body)
        + '</tbody></table></div><details><summary>Method, machine and build provenance</summary><pre>'
        + html.escape(json.dumps({"configuration": result["configuration"], **result["metadata"]}, indent=2))
        + '</pre></details></html>')


def main():
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    if sys.platform != "linux":
        raise ValueError("broker benchmarks currently require Linux")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iterations", type=int, default=1000)
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=100)
    parser.add_argument("--smoke", action="store_true", help="20 operations/client, two samples, five warmups")
    parser.add_argument("--upstream", type=Path, default=ROOT / ".dep/bin/ubusd")
    parser.add_argument("--output", type=Path, default=ROOT / "dist/benchmarks")
    args = parser.parse_args()
    if args.smoke:
        args.iterations, args.samples, args.warmup = 20, 2, 5
    if not 1 <= args.iterations <= 1_000_000 or not 1 <= args.samples <= 100 or not 0 <= args.warmup <= 1_000_000:
        parser.error("iterations must be 1..1000000, samples 1..100, warmup 0..1000000")
    upstream = args.upstream.resolve()
    if not upstream.is_file():
        parser.error("build the pinned reference first: tools/ubusd-builder/build.sh")
    binaries, profiles = build()
    provenance = metadata(binaries, profiles, upstream)
    environment = {**os.environ, "UBUS_ZERO_UBUSD": str(upstream), "UBUS_ZERO_UBUSD_RS": str(binaries["ubusd"])}
    comparisons = []
    for case_index, case in enumerate(CASES):
        samples = {"upstream": [], "rust": []}
        workload, clients, payload_bytes = case
        for repeat in range(args.samples):
            order = ("upstream", "rust") if (repeat + case_index) % 2 == 0 else ("rust", "upstream")
            for broker in order:
                print(f"{label(case)}: {broker}, sample {repeat + 1}/{args.samples}", file=sys.stderr, flush=True)
                invocation = [str(binaries["ubus-bench"]), broker, workload, str(args.iterations),
                              str(args.warmup), str(clients), str(payload_bytes)]
                sample = run_worker(invocation, environment)
                validate(sample, broker, case, args.iterations, args.warmup)
                samples[broker].append(sample)
        comparisons.append({"workload": workload, "clients": clients, "payload_bytes": payload_bytes,
                            **{broker: summarize(arm) for broker, arm in samples.items()}, "samples": samples})
    result = {"schema_version": 1, "configuration": {"iterations": args.iterations, "samples": args.samples,
              "warmup": args.warmup, "smoke": args.smoke}, "metadata": provenance, "comparisons": comparisons}
    write_reports(args.output, result)
    print(f"Reports: {args.output.resolve()}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise SystemExit(f"benchmark failed: {error}") from None
    except KeyboardInterrupt:
        raise SystemExit(130) from None
