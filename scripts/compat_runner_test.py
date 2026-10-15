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


def write_handler_probe(root, status=0, record_name="handler.json", detached_output=True):
    """Use the real process_group(0) contract of ubus-client's handlers."""
    records = root / record_name if record_name is not None else None
    handler = root / "handler.py"
    handler.write_text(
        "import json,os,pathlib,subprocess,sys,time\n"
        "suite=int(sys.argv[2])\n"
        "child=subprocess.Popen(['sleep','60'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n"
        "pids=[suite,os.getpid(),child.pid]\n"
        "record={'pids':pids,'sessions':[os.getsid(pid) for pid in pids],"
        "'groups':[os.getpgid(pid) for pid in pids]}\n"
        "path=pathlib.Path(sys.argv[1]); temporary=path.with_suffix('.tmp')\n"
        "temporary.write_text(json.dumps(record)); temporary.replace(path)\n"
        "time.sleep(60)\n"
    )
    probe = root / "probe"
    record_expression = (
        f"pathlib.Path({str(records)!r})" if records is not None else
        f"pathlib.Path({str(root)!r})/(os.environ['UBUS_ZERO_FAULT']+'.json')"
    )
    output_options = ",stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL" if detached_output else ""
    probe.write_text(
        f"#!{sys.executable}\nimport json,os,pathlib,signal,subprocess,sys,time\n"
        "assert signal.getsignal(signal.SIGINT)!=signal.SIG_IGN\n"
        f"record={record_expression}\n"
        f"subprocess.Popen([{sys.executable!r},{str(handler)!r},str(record),str(os.getpid())],"
        f"process_group=0,start_new_session=False{output_options})\n"
        "deadline=time.monotonic()+5\n"
        "while not record.exists():\n"
        " if time.monotonic()>=deadline: raise RuntimeError('handler did not start')\n"
        " time.sleep(.01)\n"
        "print(record.read_text(),flush=True)\n"
        + ("time.sleep(60)\n" if status is None else f"sys.exit({status})\n")
    )
    probe.chmod(0o700)
    return probe, records


def stop_probe_processes(records):
    if records.exists():
        for pid in json.loads(records.read_text())["pids"]:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def write_fake_workspace(root, phase="normal", build_status=0, docs_status=0):
    """Exercise actual runner/fault processes without compiling Rust."""
    tools = root / "tools"
    tools.mkdir()
    for name in ("ubus-compat-check.py", "ubus-fault-check.sh"):
        shutil.copyfile(MODULE_PATH.parent / name, tools / name)
    probe, _ = write_handler_probe(root, status=None, record_name=None)
    phase_probe = root / "phase-probe"
    shutil.copyfile(probe, phase_probe)
    phase_probe.chmod(0o700)
    if phase != "suite":
        probe.write_text(f"#!{sys.executable}\nprint('fake test complete')\n")
    cargo = tools / "cargo.sh"
    cargo.write_text(
        f"#!{sys.executable}\nimport json,os,pathlib,sys\n"
        "root=pathlib.Path(__file__).resolve().parent.parent\n"
        f"phase={phase!r}\n"
        "if ('--no-run' in sys.argv and phase=='build') or ('--doc' in sys.argv and phase=='doc'):\n"
        " os.execv(str(root/'phase-probe'),[str(root/'phase-probe')])\n"
        "if '--no-run' in sys.argv:\n"
        " for name,kind,test,path in [('ubusd','bin',False,root/'ubusd'),('probe','test',True,root/'probe')]:\n"
        "  print(json.dumps({'reason':'compiler-artifact','target':{'name':name,'kind':[kind]},"
        "'profile':{'test':test},'executable':str(path)}))\n"
        f" sys.exit({build_status})\n"
        "if '--doc' in sys.argv:\n"
        " print('fake doctest output',flush=True)\n"
        f" sys.exit({docs_status})\n"
    )
    cargo.chmod(0o700)
    return tools


class CompatRunnerTest(unittest.TestCase):
    def assert_handler_session_stopped(self, record):
        suite, handler, nested = record["pids"]
        self.assertEqual(record["sessions"], [suite, suite, suite])
        self.assertEqual(record["groups"], [suite, handler, handler])
        self.assertNotEqual(suite, handler)
        for pid in (suite, handler, nested):
            deadline = time.monotonic() + 2
            while True:
                try:
                    stat = Path(f"/proc/{pid}/stat").read_bytes()
                    state = stat[stat.rfind(b")") + 2:].split()[0]
                except (FileNotFoundError, ProcessLookupError):
                    state = None
                if state in (None, b"Z") or time.monotonic() >= deadline:
                    break
                time.sleep(.01)
            self.assertIn(state, (None, b"Z"), f"handler session member {pid} survived")

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
             patch.object(runner, "kill_session"):
            run.return_value.communicate.return_value = ("named regression failed\n", None)
            run.return_value.returncode = 101
            label, code, output, _ = runner.run_test("/tmp/test", "fd", {}, 8, 60)
        self.assertEqual((label, code, output), ("fd", 101, "named regression failed\n"))
        self.assertEqual(run.call_args.args[0], ["/tmp/test", "--quiet", "--test-threads", "8"])

    def test_timeout_is_a_failure_with_partial_diagnostics(self):
        with patch.object(runner.subprocess, "Popen") as spawn, \
             patch.object(runner, "kill_session") as kill:
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

    @unittest.skipUnless(sys.platform == "linux", "native Linux session cleanup")
    def test_handler_groups_and_nested_children_stop_on_exit_or_timeout(self):
        for status in (0, 7, None):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                probe, records = write_handler_probe(root, status)
                # A different suite/session must remain untouched by cleanup.
                unrelated = subprocess.Popen(["sleep", "60"], start_new_session=True)
                try:
                    _, code, output, _ = runner.run_test(
                        str(probe), "handler", os.environ.copy(), 1, 1 if status is None else 5)
                    self.assertEqual(code, 124 if status is None else status)
                    record = json.loads(output.splitlines()[0])
                    self.assert_handler_session_stopped(record)
                    self.assertIsNone(unrelated.poll(), "cleanup killed another suite/session")
                finally:
                    stop_probe_processes(records)
                    unrelated.kill()
                    unrelated.wait()

    @unittest.skipUnless(sys.platform == "linux", "native Linux Cargo cleanup")
    def test_cargo_capture_retains_json_and_status_without_orphan_pipe_hangs(self):
        for status in (0, 7):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                # Its handler keeps Cargo's stdout open. Temporary-file JSON
                # capture permits cleanup after Cargo exits, without EOF waits.
                probe, records = write_handler_probe(Path(directory), status, detached_output=False)
                try:
                    result = runner.run_cargo([str(probe), "--no-run"], os.environ.copy(), capture_stdout=True)
                    self.assertEqual(result.returncode, status)
                    record = json.loads(result.stdout)
                    self.assertEqual(record, json.loads(records.read_text()))
                    self.assert_handler_session_stopped(record)
                    self.assertFalse(runner.RUNNING)
                finally:
                    stop_probe_processes(records)

    @unittest.skipUnless(sys.platform == "linux", "native Linux Cargo phases")
    def test_cargo_phase_failures_and_doctest_output_are_preserved(self):
        for build_status, docs_status, expected in ((0, 0, 0), (7, 0, 7), (0, 7, 1)):
            with self.subTest(build=build_status, docs=docs_status), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                tools = write_fake_workspace(root, build_status=build_status, docs_status=docs_status)
                env = os.environ.copy()
                env.update(UBUS_ZERO_TEST_JOBS="1", RUST_TEST_THREADS="1")
                result = subprocess.run([sys.executable, str(tools / "ubus-compat-check.py")],
                    cwd=root, env=env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                self.assertEqual("fake doctest output" in result.stdout, build_status == 0)
                if docs_status:
                    self.assertIn("Failed: doctests", result.stderr)

    @unittest.skipUnless(sys.platform == "linux", "native Linux process cleanup")
    def test_fault_profile_interrupt_stops_both_runners_and_their_children(self):
        for phase in ("build", "suite", "doc"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                tools = write_fake_workspace(root, phase=phase)
                env = os.environ.copy()
                env.update(UBUS_ZERO_TEST_JOBS="1", RUST_TEST_THREADS="1")
                process = subprocess.Popen(["bash", str(tools / "ubus-fault-check.sh")],
                    cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                try:
                    deadline = time.monotonic() + 5
                    records = [root / f"{profile}.json" for profile in ("fragment=3", "eintr_every=3")]
                    while not all(path.exists() and path.stat().st_size for path in records):
                        if time.monotonic() >= deadline:
                            self.fail("both fault profiles did not start")
                        time.sleep(.01)
                    process.terminate()
                    output, _ = process.communicate(timeout=5)
                    self.assertEqual(process.returncode, 143, output)
                    for path in records:
                        self.assert_handler_session_stopped(json.loads(path.read_text()))
                finally:
                    for path in root.glob("*.json"):
                        stop_probe_processes(path)
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
        with patch.object(runner, "run_cargo", side_effect=[build, docs]) as run, \
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
        with patch.object(runner, "run_cargo", return_value=build) as run, \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(runner.main(), 1)
        self.assertEqual(run.call_count, 1)

    def test_cross_runner_keeps_cargo_execution_and_thread_override(self):
        result = subprocess.CompletedProcess([], 7)
        with patch.object(runner.sys, "platform", "darwin"), \
             patch.object(runner, "run_cargo", return_value=result) as run, \
             patch.dict(runner.os.environ, {"RUST_TEST_THREADS":"8"}, clear=True):
            self.assertEqual(runner.main(), 7)
        self.assertEqual(run.call_args.args[0],
                         [str(runner.ROOT / "tools/cargo.sh"), "test", "--workspace", "--all-features", "--no-fail-fast"])
        self.assertEqual(run.call_args.kwargs["env"]["RUST_TEST_THREADS"], "8")


if __name__ == "__main__":
    unittest.main()
