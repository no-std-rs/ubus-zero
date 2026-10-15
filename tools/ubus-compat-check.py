#!/usr/bin/env python3
"""Run the strict workspace suite with independent test binaries in parallel.

No fixture or process environment is shared between test cases. Build once,
then run every executable reported by Cargo, retaining every failure. Doctests
still run through Cargo. Override UBUS_ZERO_TEST_JOBS, RUST_TEST_THREADS, or
UBUS_ZERO_TEST_TIMEOUT (seconds per executable) to tune the resource budget.
"""

import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time


ROOT = Path(__file__).resolve().parent.parent
RUNNING = set()
RUNNING_LOCK = threading.Lock()
STOP = threading.Event()


def session_groups(session_id):
    """Find live Linux groups in the suite's SID, even after reparenting."""
    groups = set()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            # comm may contain spaces and ')' characters. The numeric fields
            # start after its final ')': state, ppid, pgrp, session, ...
            stat = (entry / "stat").read_bytes()
            fields = stat[stat.rfind(b")") + 2:].split()
            if int(fields[3]) == session_id and fields[0] not in (b"Z", b"X"):
                groups.add(int(fields[2]))
        except (OSError, ValueError, IndexError):
            # A task can exit between enumerating /proc and reading stat.
            continue
    return groups


def kill_session(process):
    """Stop the suite's private session, including handler-created groups.

    ubus-client handlers use process_group(0), so killing only the libtest
    PGID leaves those handlers alive. They retain libtest's SID, including
    after their parents exit. Ordinary fixture children stay in this session;
    deliberately creating a new session is outside this cleanup contract.
    """
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    if sys.platform != "linux":
        return
    deadline = time.monotonic() + 1
    while True:
        groups = session_groups(process.pid)
        if not groups:
            return
        for group in groups:
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        # Re-scan to catch children forked just before a parent was stopped.
        # Bound cleanup if the kernel cannot yet finish an uninterruptible IO.
        if time.monotonic() >= deadline:
            print(f"Suite session {process.pid} still has live groups after SIGKILL: "
                  f"{sorted(groups)}", file=sys.stderr, flush=True)
            return
        time.sleep(.01)


def positive_env(name, default):
    value = int(os.environ.get(name, default))
    if value < 1:
        raise ValueError(f"{name} must be a positive integer")
    return value


def test_executables(output):
    executables = {}
    for line in output.splitlines():
        message = json.loads(line)
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("profile", {}).get("test")
            and message.get("executable")
        ):
            target = message["target"]
            executables[message["executable"]] = (
                f"{target['name']} ({','.join(target['kind'])})"
            )
    return list(executables.items())


def broker_executable(output):
    for line in output.splitlines():
        message = json.loads(line)
        target = message.get("target", {})
        if (
            message.get("reason") == "compiler-artifact"
            and target.get("name") == "ubusd"
            and target.get("kind") == ["bin"]
            and not message.get("profile", {}).get("test")
            and message.get("executable")
        ):
            return message["executable"]
    return None


def run_test(executable, label, env, threads, timeout):
    started = time.monotonic()
    try:
        if STOP.is_set():
            return label, 130, "Cancelled\n", 0
        process = subprocess.Popen(
            [executable, "--quiet", "--test-threads", str(threads)],
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            start_new_session=True,
        )
        with RUNNING_LOCK:
            RUNNING.add(process)
        try:
            if STOP.is_set():
                kill_session(process)
            try:
                output, _ = process.communicate(timeout=timeout)
                status = process.returncode
            except subprocess.TimeoutExpired:
                # Killing only libtest bypasses harness Drop and leaves its
                # brokers/rpcd/CLI children alive. Each suite owns a session.
                kill_session(process)
                output, _ = process.communicate()
                output += f"\nTimed out after {timeout}s\n"
                status = 124
            except BaseException:
                kill_session(process)
                process.communicate()
                raise
            return label, status, output, time.monotonic() - started
        finally:
            # A panicking suite can drop a plain std::process::Child without
            # stopping it. Descendants with detached output do not keep
            # communicate() open, so stop the suite's whole private session
            # even when the executable itself has already returned.
            kill_session(process)
            with RUNNING_LOCK:
                RUNNING.discard(process)
            if process.stdout:
                process.stdout.close()
    except OSError as error:
        return label, 127, f"Could not execute test binary: {error}\n", time.monotonic() - started


def main():
    STOP.clear()
    # A shell background job inherits ignored INT/QUIT. Restore them before
    # exec so brokers can install their usual handlers and lifecycle tests
    # exercise the same signals as foreground runs. Python's INT handler
    # becomes SIG_DFL in each exec'd child.
    signal.signal(signal.SIGINT, signal.default_int_handler)
    signal.signal(signal.SIGQUIT, signal.SIG_DFL)
    env = os.environ.copy()
    env["UBUS_ZERO_COMPAT"] = "require"
    try:
        cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
        jobs = positive_env("UBUS_ZERO_TEST_JOBS", min(4, cpus))
        threads = positive_env("RUST_TEST_THREADS", max(2, (cpus * 2 + jobs - 1) // jobs))
        timeout = positive_env("UBUS_ZERO_TEST_TIMEOUT", 300)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2

    cargo = [str(ROOT / "tools/cargo.sh"), "test", "--workspace", "--all-features"]
    if sys.platform != "linux":
        # Cross builds must execute through the configured target runner.
        env["RUST_TEST_THREADS"] = str(threads)
        return subprocess.call([*cargo, "--no-fail-fast"], cwd=ROOT, env=env)

    started = time.monotonic()
    build = subprocess.run(
        [*cargo, "--no-run", "--message-format=json-render-diagnostics"],
        cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True, check=False,
    )
    if build.returncode:
        return build.returncode
    executables = test_executables(build.stdout)
    if not executables:
        print("Cargo reported no test executables", file=sys.stderr)
        return 1
    broker = broker_executable(build.stdout)
    if not broker:
        print("Cargo reported no fresh ubusd executable", file=sys.stderr)
        return 1
    # CARGO_TARGET_DIR may point outside the workspace. Select Cargo's actual
    # artifact instead of silently testing a stale target/debug/ubusd fallback.
    env.setdefault("UBUS_ZERO_UBUSD_RS", broker)

    print(f"Strict compatibility: {len(executables)} binaries, {jobs} concurrent binaries, "
          f"{threads} threads per binary", flush=True)
    failures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        pending = [pool.submit(run_test, path, label, env, threads, timeout)
                   for path, label in executables]
        try:
            for future in concurrent.futures.as_completed(pending):
                label, status, output, elapsed = future.result()
                print(f"\n== {label}: {'PASS' if status == 0 else 'FAIL'} ({elapsed:.2f}s)\n"
                      f"{output}", flush=True)
                if status:
                    failures.append(label)
        except KeyboardInterrupt:
            STOP.set()
            for future in pending:
                future.cancel()
            with RUNNING_LOCK:
                for process in RUNNING:
                    kill_session(process)
            return 130

    docs = subprocess.run([*cargo, "--doc", "--no-fail-fast"], cwd=ROOT, env=env, check=False)
    if docs.returncode:
        failures.append("doctests")
    print(f"Strict compatibility finished in {time.monotonic() - started:.2f}s; "
          f"{len(failures)} failed suite(s)", flush=True)
    if failures:
        print("Failed: " + ", ".join(failures), file=sys.stderr)
    return int(bool(failures))


if __name__ == "__main__":
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    raise SystemExit(main())
