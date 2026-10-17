"""Measurement oracles: concurrent wall time, exact samples and safe report rendering."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("benchmark", Path(__file__).resolve().parents[1] / "tools/ubus-benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
    def sample(self):
        return {"schema_version": 1, "broker": "rust", "workload": "echo", "clients": 4,
                "payload_bytes": 4096, "iterations": 2, "warmup": 1, "operations": 8,
                "elapsed_ns": 1_000_000_000, "latency_ns": [500_000_000] * 8}

    def test_concurrent_throughput_uses_wall_time_instead_of_summed_latencies(self):
        sample = self.sample()
        benchmark.validate(sample, "rust", ("echo", 4, 4096), 2, 1)
        stats = benchmark.summarize([sample])
        self.assertEqual(stats["operations_per_second"], 8)
        self.assertEqual(stats["p99_us"], 500_000)

    def test_repeated_throughput_uses_median_and_percentiles_retain_tail(self):
        sample = self.sample()
        samples = [sample, {**sample, "elapsed_ns": 2_000_000_000},
                   {**sample, "elapsed_ns": 4_000_000_000}]
        self.assertEqual(benchmark.summarize(samples)["operations_per_second"], 4)
        self.assertEqual(benchmark.percentile(list(range(1, 101)), 99), 99)
        self.assertEqual(benchmark.percentile([1, 2, 100], 99), 100)

    def test_missing_wrong_arm_or_impossible_samples_are_rejected(self):
        for change in ({"broker": "upstream"}, {"clients": True}, {"operations": 7},
                       {"latency_ns": [1]}, {"latency_ns": [0] * 8},
                       {"elapsed_ns": 0}, {"elapsed_ns": 1}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                benchmark.validate({**self.sample(), **change}, "rust", ("echo", 4, 4096), 2, 1)

    def test_reports_include_provenance_and_escape_metadata(self):
        stats = benchmark.summarize([self.sample()])
        result = {"metadata": {"commit": "<script>unsafe</script>"},
                  "configuration": {"samples": 1, "iterations": 2},
                  "comparisons": [{"workload": "echo", "clients": 4, "payload_bytes": 4096,
                                   "rust": stats, "upstream": stats}]}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            benchmark.write_reports(output, result)
            report = (output / "report.html").read_text()
            self.assertNotIn("<script>", report)
            self.assertIn("&lt;script&gt;", report)
            self.assertIn("not a timing gate", report)
            self.assertEqual(json.loads((output / "performance.json").read_text()), result)
            self.assertEqual(len((output / "summary.csv").read_text().splitlines()), 3)

    @unittest.skipUnless(sys.platform == "linux", "Linux process sessions")
    def test_timeout_reaps_the_benchmark_worker(self):
        original = subprocess.Popen
        pids = []

        def start(*args, **kwargs):
            process = original(*args, **kwargs)
            pids.append(process.pid)
            return process

        with patch.object(benchmark.processes.subprocess, "Popen", side_effect=start):
            with self.assertRaises(subprocess.TimeoutExpired):
                benchmark.run_worker([sys.executable, "-c", "import time; time.sleep(30)"],
                                     os.environ.copy(), timeout=.5)
        self.assertEqual(len(pids), 1)
        with self.assertRaises(ProcessLookupError):
            os.kill(pids[0], 0)


if __name__ == "__main__":
    unittest.main()
