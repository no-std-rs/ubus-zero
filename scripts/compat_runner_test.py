"""The parallel gate must retain every executable and propagate failures."""

import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


MODULE_PATH = Path(__file__).resolve().parent.parent / "tools/ubus-compat-check.py"
SPEC = importlib.util.spec_from_file_location("compat_runner", MODULE_PATH)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class CompatRunnerTest(unittest.TestCase):
    def test_cargo_artifacts_keep_distinct_binaries_without_duplicates(self):
        def artifact(path, test=True):
            return {"reason": "compiler-artifact", "profile": {"test": test},
                    "executable": path, "target": {"name": "ubusd", "kind": ["test"]}}

        output = "\n".join(json.dumps(item) for item in [
            {"reason": "build-finished", "success": True},
            artifact("/tmp/normal-binary", False), artifact(None),
            artifact("/tmp/first"), artifact("/tmp/first"), artifact("/tmp/second"),
        ])
        self.assertEqual([path for path, _ in runner.test_executables(output)],
                         ["/tmp/first", "/tmp/second"])

    def test_binary_failure_is_retained_with_diagnostics(self):
        with patch.object(runner.subprocess, "Popen") as run, \
             patch.object(runner, "kill_group"):
            run.return_value.communicate.return_value = ("named regression failed\n", None)
            run.return_value.returncode = 101
            label, code, output, _ = runner.run_test("/tmp/test", "fd", {}, 8, 60)
        self.assertEqual((label, code, output), ("fd", 101, "named regression failed\n"))
        self.assertEqual(run.call_args.args[0], ["/tmp/test", "--quiet", "--test-threads", "8"])

    def test_timeout_is_a_failure_with_partial_diagnostics(self):
        with patch.object(runner.subprocess, "Popen") as spawn, \
             patch.object(runner, "kill_group") as kill:
            spawn.return_value.communicate.side_effect = [
                subprocess.TimeoutExpired([], 60), ("last active case\n", None),
            ]
            _, code, output, _ = runner.run_test("/tmp/test", "fd", {}, 8, 60)
        kill.assert_called_with(spawn.return_value)
        self.assertEqual(code, 124)
        self.assertIn("last active case", output)
        self.assertIn("Timed out", output)

    @unittest.skipUnless(sys.platform == "linux", "runner's native Linux process groups")
    def test_timeout_kills_broker_grandchildren(self):
        with tempfile.TemporaryDirectory() as directory:
            probe = Path(directory) / "test"
            # A broker-like child holds the suite's stdout pipe open. Killing
            # only the suite would leave that child and its resources alive.
            probe.write_text(f"#!{sys.executable}\nimport subprocess,time\n"
                             "child=subprocess.Popen(['sleep','60'])\n"
                             "print(child.pid,flush=True)\ntime.sleep(60)\n")
            probe.chmod(0o700)
            _, code, output, _ = runner.run_test(str(probe), "hang", os.environ.copy(), 1, .2)
            self.assertEqual(code, 124)
            child = int(output.splitlines()[0])
            try:
                state = Path(f"/proc/{child}/stat").read_text().split()[2]
            except (FileNotFoundError, ProcessLookupError):
                state = None
            if state not in (None, "Z"):
                os.kill(child, signal.SIGKILL)
            self.assertIn(state, (None, "Z"), "grandchild survived suite timeout")

    @unittest.skipUnless(sys.platform == "linux", "native Linux process cleanup")
    def test_completed_suites_stop_children_with_detached_output(self):
        with tempfile.TemporaryDirectory() as directory:
            probe = Path(directory) / "test"
            for status in (0, 7):
                with self.subTest(status=status):
                    probe.write_text(f"#!{sys.executable}\nimport subprocess,sys\n"
                        "child=subprocess.Popen(['sleep','60'],"
                        "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n"
                        f"print(child.pid,flush=True)\nsys.exit({status})\n")
                    probe.chmod(0o700)
                    _, code, output, _ = runner.run_test(
                        str(probe), "finished", os.environ.copy(), 1, 5)
                    self.assertEqual(code, status)
                    child = int(output.splitlines()[0])
                    try:
                        # Group termination is asynchronous. Wait for the
                        # kernel task state, without accepting a live child.
                        deadline = time.monotonic() + 2
                        while True:
                            try:
                                state = Path(f"/proc/{child}/stat").read_text().split()[2]
                            except (FileNotFoundError, ProcessLookupError):
                                state = None
                            if state in (None, "Z") or time.monotonic() >= deadline:
                                break
                            time.sleep(.01)
                        self.assertIn(state, (None, "Z"), "child survived suite completion")
                    finally:
                        try:
                            os.kill(child, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    @unittest.skipUnless(sys.platform == "linux", "native Linux process cleanup")
    def test_fault_profile_interrupt_stops_both_runners_and_their_children(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            tools.mkdir()
            for name in ("ubus-compat-check.py", "ubus-fault-check.sh"):
                shutil.copyfile(MODULE_PATH.parent / name, tools / name)
            cargo = tools / "cargo.sh"
            cargo.write_text(f"#!{sys.executable}\nimport json,os,pathlib,sys\n"
                "root=pathlib.Path(__file__).resolve().parent.parent\n"
                "if '--no-run' in sys.argv:\n"
                " for name,kind,test,path in [('ubusd','bin',False,root/'ubusd'),"
                "('probe','test',True,root/'probe')]:\n"
                "  print(json.dumps({'reason':'compiler-artifact','target':{'name':name,"
                "'kind':[kind]},'profile':{'test':test},'executable':str(path)}))\n")
            cargo.chmod(0o700)
            probe = root / "probe"
            probe.write_text(f"#!{sys.executable}\nimport os,pathlib,signal,subprocess,time\n"
                "assert signal.getsignal(signal.SIGINT)!=signal.SIG_IGN\n"
                "child=subprocess.Popen(['sleep','60'])\n"
                "root=pathlib.Path(__file__).parent\n"
                "(root/(os.environ['UBUS_ZERO_FAULT']+'.pid')).write_text("
                "str(os.getpid())+' '+str(child.pid))\n"
                "time.sleep(60)\n")
            probe.chmod(0o700)
            env = os.environ.copy()
            env.update(UBUS_ZERO_TEST_JOBS="1", RUST_TEST_THREADS="1")
            process = subprocess.Popen(["bash", str(tools / "ubus-fault-check.sh")],
                cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            pids = []
            try:
                deadline = time.monotonic() + 5
                records = [root / f"{profile}.pid" for profile in ("fragment=3", "eintr_every=3")]
                while not all(path.exists() and path.stat().st_size for path in records):
                    if time.monotonic() >= deadline:
                        self.fail("both fault profiles did not start")
                    time.sleep(.01)
                for path in records:
                    pids.extend(map(int, path.read_text().split()))
                process.terminate()
                output, _ = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 143, output)
                for pid in pids:
                    try:
                        state = Path(f"/proc/{pid}/stat").read_text().split()[2]
                    except (FileNotFoundError, ProcessLookupError):
                        state = None
                    self.assertIn(state, (None, "Z"), f"profile child {pid} survived interrupt")
            finally:
                for path in root.glob("*.pid"):
                    for pid in map(int, path.read_text().split()):
                        try:
                            os.kill(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                if process.poll() is None:
                    process.kill()
                process.communicate()

    def test_runs_remaining_suites_and_doctests_after_failure(self):
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "executable": sys.executable, "target": {"name": "case", "kind": ["test"]}}
        broker = {"reason": "compiler-artifact", "profile": {"test": False},
                  "executable": "/fresh/ubusd", "target": {"name": "ubusd", "kind": ["bin"]}}
        build = subprocess.CompletedProcess([], 0, json.dumps(artifact) + "\n" + json.dumps(broker))
        docs = subprocess.CompletedProcess([], 0)
        with patch.object(runner.subprocess, "run", side_effect=[build, docs]) as run, \
             patch.object(runner, "run_test", return_value=("case", 101, "failure", 0)), \
             patch.dict(runner.os.environ, {}, clear=True), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(runner.main(), 1)
        self.assertEqual(run.call_args.kwargs["env"]["UBUS_ZERO_UBUSD_RS"], "/fresh/ubusd")
        self.assertIn("--doc", run.call_args.args[0])

    def test_missing_fresh_broker_is_a_failure(self):
        artifact = {"reason": "compiler-artifact", "profile": {"test": True},
                    "executable": "/tmp/test", "target": {"name": "ubusd", "kind": ["bin"]}}
        build = subprocess.CompletedProcess([], 0, json.dumps(artifact))
        with patch.object(runner.subprocess, "run", return_value=build) as run, \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(runner.main(), 1)
        self.assertEqual(run.call_count, 1)


if __name__ == "__main__":
    unittest.main()
