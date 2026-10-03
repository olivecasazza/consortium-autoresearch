"""Stop in-process ClusterShell CLI code from hard-exiting the pytest process.

Why this exists
---------------
`ClusterShell.CLI.Clush.main()` starts with::

    sys.excepthook = clush_excepthook

and later, once a :class:`ClusterShell.Task` exists::

    task.excepthook = sys.excepthook

so the CLI's "abort the whole process" handler becomes the *process-wide*
excepthook and the excepthook of every ClusterShell task thread. That handler
ends in :func:`clush_exit` with no ``task`` argument, whose no-task branch is::

    # Use os._exit to avoid threads cleanup
    os._exit(status)

Upstream that is correct — clush *is* the process. Inside a pytest run it is
not: any exception raised in a ClusterShell worker thread reaches
``clush_excepthook``, which calls ``os._exit()`` and kills the runner from
underneath pytest. The observable result is a truncated test line, exit code
1, no summary block, and no ``--junit-xml`` file at all, so the scorecard's
JUnit gate has nothing to publish.

This plugin makes that exit a test outcome instead. It is deliberately scoped
to the two things that must not leak into a test run:

1. ``ClusterShell.CLI.Clush.clush_exit`` raises ``SystemExit`` instead of
   calling ``os._exit``. The task-less branch is the only one that reaches
   ``os._exit``; every in-process call site of the task-carrying branch already
   used ``sys.exit()``, so raising here is behaviour-preserving for
   ``tests/TLib.py:CLI_main``, which converts ``SystemExit`` into an ``rc`` the
   test then asserts on.
2. ``sys.excepthook`` is restored after each test, so a hook installed by one
   test cannot decide the fate of a later one.

A hard exit that arrives from a *non-main* thread is recorded and re-raised at
teardown as an explicit test failure, so the condition stays visible instead of
degenerating into a mysteriously empty or green test.

This lives in ``harness/`` rather than ``lib/ClusterShell/`` on purpose:
``harness/sync_upstream_tests.sh`` rsyncs ``tests/`` and ``lib/ClusterShell/``
from upstream with ``--delete`` on every scorecard run, so a fix in either of
those directories would be erased before pytest ever ran.
"""

from __future__ import annotations

import sys
import threading
import traceback

import pytest

# Process-wide on purpose: a ClusterShell worker thread is a *different*
# thread than the one running the test, so per-thread state here would file
# the record somewhere the fixture teardown can never read it. pytest-xdist
# gives each worker its own process, so a plain list is single-writer in every
# configuration we run.
_INTERCEPTS = []


def _record(status, in_main_thread) -> None:
    exc_type, exc_value, tb = sys.exc_info()
    detail = "".join(traceback.format_exception(exc_type, exc_value, tb)) \
        if exc_type is not None else "(no active exception)"
    _INTERCEPTS.append({
        "status": status,
        "in_main_thread": in_main_thread,
        "thread": threading.current_thread().name,
        "detail": detail,
    })


def _make_clush_exit(original):
    def clush_exit(status, task=None):
        """``clush_exit`` that can never take the test runner down with it."""
        if task is not None:
            # Unmodified upstream semantics: this already used sys.exit().
            task.abort()
            task.join()
            raise SystemExit(status)

        # Upstream: flush stdio, then os._exit(status).
        # Here: record it, flush stdio, and unwind the current execution
        # context instead of the interpreter.
        _record(status, threading.current_thread() is threading.main_thread())
        for stream in (sys.stdout, sys.stderr):
            try:
                stream.flush()
            except (OSError, ValueError):
                pass
        raise SystemExit(status)

    clush_exit.__wrapped__ = original
    clush_exit.__doc__ = (
        "CON-115: drop-in ClusterShell.CLI.Clush.clush_exit that raises "
        "SystemExit instead of calling os._exit().\n\n" + (original.__doc__ or "")
    )
    return clush_exit


@pytest.fixture(autouse=True)
def _clustershell_no_hard_exit():
    """Autouse for every test: clush must not os._exit() the pytest process."""
    try:
        from ClusterShell.CLI import Clush
    except ImportError:
        # No ClusterShell on the path (e.g. Rust-backend-only runs): nothing
        # to guard.
        yield
        return

    saved_excepthook = sys.excepthook
    saved_clush_exit = Clush.clush_exit
    del _INTERCEPTS[:]
    Clush.clush_exit = _make_clush_exit(saved_clush_exit)
    try:
        yield
    finally:
        Clush.clush_exit = saved_clush_exit
        sys.excepthook = saved_excepthook
        from_worker = [i for i in _INTERCEPTS if not i["in_main_thread"]]
        del _INTERCEPTS[:]
        if from_worker:
            pytest.fail(
                "ClusterShell worker thread (%s) asked clush to exit the process "
                "(CON-115 guard). Upstream's clush_excepthook calls os._exit(%d) "
                "here, which would have killed the pytest runner before it could "
                "write a summary or a JUnit report.\n%s"
                % (from_worker[-1]["thread"], from_worker[-1]["status"],
                   from_worker[-1]["detail"]),
                pytrace=False,
            )


def pytest_report_header(config):
    return "consortium: ClusterShell hard-exit guard active (harness/scorecard_guard.py)"


#
# ── Self-test ───────────────────────────────────────────────────────────────
#
# The regression this module exists for is a *process* regression: pytest used
# to be killed mid-suite, so no summary and no --junit-xml file. A unit test
# cannot observe that — the test would have to survive the thing that kills it.
# So the check runs pytest in a child process and asserts the child finished
# normally and wrote its report. Run it with `python harness/scorecard_guard.py`.
#
_PROBE = '''\
"""Generated by harness/scorecard_guard.py — proves the guard is installed."""
import sys
import unittest

import ClusterShell.CLI.Clush
from ClusterShell.CLI.Clush import clush_excepthook
from ClusterShell.CLI.Clush import main
from tests.TLib import CLI_main


class GuardProbeTest(unittest.TestCase):
    def test_worker_thread_error_is_a_test_failure(self):
        # The CLIClushTest_A.test_033_worker_pdsh_tty shape: _f_user_interaction
        # makes clush run the command in a ClusterShell Task thread, so a worker
        # error reaches clush_excepthook -> clush_exit() -> os._exit().
        setattr(ClusterShell.CLI.Clush, '_f_user_interaction', True)
        try:
            CLI_main(self, main,
                     ['clush', '-w', 'localhost', '--worker=pdsh', 'echo ok'],
                     None, b'localhost: ok\\n', 0)
        finally:
            delattr(ClusterShell.CLI.Clush, '_f_user_interaction')

    def test_excepthook_does_not_leak_into_later_tests(self):
        # clush main() assigns sys.excepthook = clush_excepthook and never
        # restores it; the guard puts it back so one test cannot decide the
        # fate of the next.
        self.assertIsNot(sys.excepthook, clush_excepthook)
'''


def _self_test() -> int:
    import os
    import re
    import shutil
    import subprocess
    import tempfile
    import xml.etree.ElementTree as ET

    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    probe_dir = tempfile.mkdtemp(prefix="scorecard-guard-probe-", dir=repo_root)
    try:
        with open(os.path.join(probe_dir, "GuardProbeTest.py"), "w") as fp:
            fp.write(_PROBE)
        # A pdsh that always fails, so the probe's worker-thread error is
        # deterministic whether or not the host has a working pdsh (the CI
        # runner does, and a real one may well succeed, which would make the
        # probe pass for the wrong reason).
        probe_bin = os.path.join(probe_dir, "bin")
        os.mkdir(probe_bin)
        pdsh = os.path.join(probe_bin, "pdsh")
        with open(pdsh, "w") as fp:
            fp.write("#!/bin/sh\nexit 7\n")
        os.chmod(pdsh, 0o755)

        report = os.path.join(probe_dir, "guard-probe.xml")
        env = dict(os.environ)
        # The probe imports tests.TLib, so the repo root has to be importable
        # regardless of how the outer pytest resolves sys.path.
        env["PYTHONPATH"] = os.pathsep.join(
            [repo_root, os.path.join(repo_root, "lib")]
            + ([env["PYTHONPATH"]] if env.get("PYTHONPATH") else []))
        env["PATH"] = os.pathsep.join([probe_bin, env.get("PATH", "")])
        proc = subprocess.run(
            [sys.executable, "-m", "pytest", probe_dir, "-v", "--timeout=60",
             "--junit-xml", report],
            cwd=repo_root, env=env, capture_output=True, text=True)
        out = proc.stdout + proc.stderr
        tail = "\n".join(out.splitlines()[-15:])

        problems = []
        if "no tests ran" in out or "collected 0 items" in out:
            problems.append("probe was not collected:\n%s" % tail)
        elif "hard-exit guard active" not in out:
            problems.append("guard plugin was not loaded by the root conftest")
        if not os.path.exists(report):
            problems.append(
                "pytest wrote no JUnit report — the runner was hard-exited "
                "before it could finish (CON-115):\n%s" % tail)
        # pytest's last line is "=== N failed, M passed in T s ===". Its absence
        # is the abort: the reporter never got to run.
        summary = re.search(r"^=+ .*(?:failed|passed).*=+$", out, re.M)
        if not summary:
            problems.append(
                "no summary line in pytest output — the runner died mid-suite "
                "(CON-115):\n%s" % tail)
        if proc.returncode not in (0, 1):
            problems.append("pytest exited %d, expected 0 or 1:\n%s"
                            % (proc.returncode, tail))
        if os.path.exists(report):
            root = ET.parse(report).getroot()
            suite = root.find("testsuite") if root.tag == "testsuites" else root
            if int(suite.get("tests", 0)) < 2:
                problems.append("probe suite reported %r tests, expected >= 2"
                                % suite.get("tests"))
            failed = int(suite.get("failures", 0)) + int(suite.get("errors", 0))
            if failed < 1:
                problems.append(
                    "probe suite reported %d failures; the worker-thread hard "
                    "exit should have been turned into a test failure"
                    % failed)

        if problems:
            print("scorecard guard self-test FAILED:")
            for problem in problems:
                print("  - %s" % problem)
            return 1

        print("scorecard guard self-test passed: a ClusterShell worker-thread "
              "hard exit became a test failure and pytest still wrote %s"
              % os.path.relpath(report, repo_root))
        return 0
    finally:
        shutil.rmtree(probe_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(_self_test())
