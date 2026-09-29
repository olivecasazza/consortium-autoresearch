#!/usr/bin/env python3
"""test_graduation_gate.py — self-tests for the CON-4 graduation gate.

These are the tests that make the gate trustworthy: they prove it FAILs on the
conditions it is supposed to catch (including the CON-198 empty-Rust-leg rot)
and, just as importantly, that it can still PASS. A gate that only ever fails
carries no information.

    python -m pytest harness/test_graduation_gate.py -q
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parent
GATE = HARNESS_DIR / "graduation_gate.py"

CONFIG = (HARNESS_DIR / "graduation-gate.toml").read_text()
EXEMPTIONS_HEADER = (HARNESS_DIR / "graduation-exemptions.toml").read_text().split("[[exemption]]")[0]


def junit(tests: list[tuple[str, str]], *, collection_errors: int = 0) -> str:
    """Build a JUnit XML string from (classname, name, status) triples."""
    body = []
    for classname, name, status in tests:
        if status == "fail":
            body.append(f'    <testcase classname="{classname}" name="{name}"><failure message="x"/></testcase>')
        elif status == "error":
            body.append(f'    <testcase classname="{classname}" name="{name}"><error message="x"/></testcase>')
        elif status == "skip":
            body.append(f'    <testcase classname="{classname}" name="{name}"><skipped/></testcase>')
        else:
            body.append(f'    <testcase classname="{classname}" name="{name}"/>')
    for i in range(collection_errors):
        body.append(f'    <testcase classname="" name="tests.Module{i}"><error message="collection failure"/></testcase>')
    n = len(tests)
    failures = sum(1 for t in tests if t[2] in ("fail", "error"))
    return (
        f'<?xml version="1.0" encoding="utf-8"?>\n'
        f'<testsuite name="pytest" tests="{n}" failures="{failures}" errors="0">\n'
        + "\n".join(body)
        + "\n</testsuite>\n"
    )


def rust_junit(names: list[str], failures: list[str] | None = None) -> str:
    """Build a cargo_to_junit.py-shaped Rust-unit JUnit file."""
    failures = failures or []
    body = [
        f'    <testcase name="{n}" classname="consortium">'
        + ('<failure message="x"/>' if n in failures else "")
        + "</testcase>"
        for n in names
    ]
    return (
        f'<?xml version="1.0" encoding="utf-8"?>\n'
        f'<testsuite name="consortium" tests="{len(names)}" failures="{len(failures)}" errors="0">\n'
        + "\n".join(body)
        + "\n</testsuite>\n"
    )


def run_gate(
    tmp_path: Path,
    *,
    mapping: str,
    rust_unit: str,
    python_original: str | None = None,
    python_rust: str | None = None,
    config: str = CONFIG,
    exemptions: str = EXEMPTIONS_HEADER,
) -> tuple[int, str, dict]:
    (tmp_path / "TEST_MAPPING.toml").write_text(mapping)
    (tmp_path / "tests").mkdir(exist_ok=True)
    (tmp_path / "results").mkdir(exist_ok=True)
    (tmp_path / "results" / "rust-unit.xml").write_text(rust_unit)
    if python_original is not None:
        (tmp_path / "results" / "python-original.xml").write_text(python_original)
    if python_rust is not None:
        (tmp_path / "results" / "python-rust.xml").write_text(python_rust)
    (tmp_path / "graduation-gate.toml").write_text(config)
    (tmp_path / "graduation-exemptions.toml").write_text(exemptions)
    json_out = tmp_path / "report.json"

    proc = subprocess.run(
        [
            sys.executable,
            str(GATE),
            "--mapping", str(tmp_path / "TEST_MAPPING.toml"),
            "--tests-dir", str(tmp_path / "tests"),
            "--results-dir", str(tmp_path / "results"),
            "--config", str(tmp_path / "graduation-gate.toml"),
            "--exemptions", str(tmp_path / "graduation-exemptions.toml"),
            "--json-out", str(json_out),
        ],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
        # These self-tests run in CI, in the same job and environment as the
        # real gate step. Without scrubbing, every one of them would append a
        # bogus `graduation_gate=` output and step-summary table, and the real
        # signal would arrive buried in synthetic ones.
        env={k: v for k, v in os.environ.items() if k not in ("GITHUB_OUTPUT", "GITHUB_STEP_SUMMARY")},
    )
    report = json.loads(json_out.read_text()) if json_out.exists() else {}
    return proc.returncode, proc.stdout + proc.stderr, report


# ── a fully-ported world: everything green, must PASS ────────────────────

MAPPED = textwrap.dedent(
    """
    [upstream]
    repo = "cea-hpc/clustershell"

    [TaskFooTest]
    test_a = ["task::tests::test_a"]
    test_b = ["task::tests::test_b"]

    [TreeBarTest]
    test_c = ["tree::tests::test_c"]

    [UnrelatedTest]
    test_d = []
    """
)


def write_upstream(py_classes: dict[str, list[str]], tmp_path: Path) -> None:
    (tmp_path / "tests").mkdir(parents=True, exist_ok=True)
    for cls, methods in py_classes.items():
        body = "\n".join(f"    def {m}(self):\n        pass" for m in methods)
        (tmp_path / "tests" / f"{cls}.py").write_text(
            "import unittest\n\n\n"
            f"class {cls}(unittest.TestCase):\n{body}\n"
        )


ALL_GREEN = {
    "TaskFooTest": ["test_a", "test_b"],
    "TreeBarTest": ["test_c"],
    "UnrelatedTest": ["test_d"],
}

# One world where every upstream method is ported, and one that sits exactly on
# the floor (4 of 5 methods). Both must PASS; 3 of 4 must not.
MAPPED_ALL = textwrap.dedent(
    """
    [upstream]
    repo = "cea-hpc/clustershell"

    [TaskFooTest]
    test_a = ["task::tests::test_a"]
    test_b = ["task::tests::test_b"]

    [TreeBarTest]
    test_c = ["tree::tests::test_c"]

    [UnrelatedTest]
    test_d = ["misc::tests::test_d"]
    test_e = ["misc::tests::test_e"]
    """
)


def py_all_pass(py_classes: dict[str, list[str]]) -> str:
    return junit(
        [(f"tests.{cls}", m, "pass") for cls, methods in py_classes.items() for m in methods]
    )


def test_gate_passes_when_every_method_is_ported(tmp_path):
    py_classes = {**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]}
    write_upstream(py_classes, tmp_path)
    rust = rust_junit([
        "task::tests::test_a", "task::tests::test_b",
        "tree::tests::test_c", "misc::tests::test_d", "misc::tests::test_e",
    ])
    py = py_all_pass(py_classes)
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL, rust_unit=rust, python_original=py, python_rust=py
    )
    assert code == 0, out
    assert "GRADUATION GATE: PASS" in out
    assert report["verdict"] == "PASS"
    assert report["methods_total"] == 5
    assert report["methods_covered"] == 5
    assert all(c["passed"] for c in report["checks"])


def test_gate_passes_exactly_on_the_floor(tmp_path):
    """80% of 5 is 4. The floor is inclusive."""
    py_classes = {**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]}
    write_upstream(py_classes, tmp_path)
    mapping = MAPPED_ALL.replace('test_e = ["misc::tests::test_e"]\n', "")
    rust = rust_junit([
        "task::tests::test_a", "task::tests::test_b",
        "tree::tests::test_c", "misc::tests::test_d",
    ])
    py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
        ("tests.TreeBarTest", "test_c", "pass"),
        ("tests.UnrelatedTest", "test_d", "pass"),
        ("tests.UnrelatedTest", "test_e", "pass"),
    ])
    code, out, report = run_gate(
        tmp_path, mapping=mapping, rust_unit=rust, python_original=py, python_rust=py
    )
    assert code == 0, out
    assert report["methods_covered"] == 4
    assert "coverage=80.0%" in out


def test_gate_fails_below_the_floor(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    assert "GRADUATION GATE: FAIL" in out
    assert "coverage" in out
    assert report["checks"][0]["name"] == "coverage"
    assert not report["checks"][0]["passed"]


# ── criterion 3: the CON-198 rot ─────────────────────────────────────────


def test_gate_fails_on_an_empty_rust_leg(tmp_path):
    """CON-198 made the Rust leg run `-p consortium`, measuring nothing and
    writing tests="0". The gate must not read that as a healthy signal."""
    write_upstream(ALL_GREEN, tmp_path)
    empty = rust_junit([])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=MAPPED, rust_unit=empty, python_original=py, python_rust=py)
    assert code == 1
    signal = next(c for c in report["checks"] if c["name"] == "trustworthy-signal")
    assert not signal["passed"]
    assert "consortium-crate" in signal["detail"]


def test_gate_fails_on_a_missing_rust_leg(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=MAPPED, rust_unit="", python_original=py, python_rust=py)
    assert code == 1
    signal = next(c for c in report["checks"] if c["name"] == "trustworthy-signal")
    assert not signal["passed"]


def test_gate_fails_on_a_collection_error(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")], collection_errors=1)
    code, out, report = run_gate(tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    signal = next(c for c in report["checks"] if c["name"] == "trustworthy-signal")
    assert not signal["passed"]
    assert "collection error" in signal["detail"]


# ── criterion 1b: regressions ────────────────────────────────────────────


def test_gate_fails_on_a_single_regression(tmp_path):
    """One method that passes on the python baseline and fails on rust is
    enough. There is no tolerance knob."""
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c"])
    orig = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
        ("tests.TreeBarTest", "test_c", "pass"),
    ])
    rust_py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "fail"),
        ("tests.TreeBarTest", "test_c", "pass"),
    ])
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED, rust_unit=rust, python_original=orig, python_rust=rust_py
    )
    assert code == 1
    reg = next(c for c in report["checks"] if c["name"] == "regressions")
    assert not reg["passed"]
    assert "1 method" in reg["measured"]


# ── criterion 2: critical paths and reviewed decisions ────────────────────


def test_gate_fails_on_an_unmapped_critical_class(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b"])
    py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
    ])
    code, out, report = run_gate(tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    crit = next(c for c in report["checks"] if c["name"] == "critical-paths")
    assert not crit["passed"]
    assert "TreeBarTest" in crit["detail"]


def test_a_reviewed_decision_satisfies_criterion_2(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b"])
    py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
    ])
    exemptions = EXEMPTIONS_HEADER + textwrap.dedent(
        """
        [[exemption]]
        class = "TreeBarTest"
        decision = "accept"
        reason = "covered by native tree tests, reviewed"
        reviewer = "CTO"
        date = "2026-07-18"
        issue = "CON-199"
        """
    )
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py,
        exemptions=exemptions,
    )
    crit = next(c for c in report["checks"] if c["name"] == "critical-paths")
    assert crit["passed"], out
    assert "TreeBarTest" in report["exemptions"]


@pytest.mark.parametrize("missing", ["class", "decision", "reason", "reviewer", "date", "issue"])
def test_an_incomplete_decision_does_not_satisfy_criterion_2(tmp_path, missing):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b"])
    py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
    ])
    fields = {
        "class": "TreeBarTest",
        "decision": "accept",
        "reason": "covered elsewhere",
        "reviewer": "CTO",
        "date": "2026-07-18",
        "issue": "CON-199",
    }
    fields.pop(missing)
    body = "\n".join(f'  {k} = "{v}"' for k, v in fields.items())
    exemptions = EXEMPTIONS_HEADER + f"\n[[exemption]]\n{body}\n"
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py,
        exemptions=exemptions,
    )
    assert code == 1
    assert any(missing in p for p in report["problems"]), report["problems"]


def test_a_decision_for_a_non_critical_class_is_rejected(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c"])
    py = junit([
        ("tests.TaskFooTest", "test_a", "pass"),
        ("tests.TaskFooTest", "test_b", "pass"),
        ("tests.TreeBarTest", "test_c", "pass"),
    ])
    exemptions = EXEMPTIONS_HEADER + textwrap.dedent(
        """
        [[exemption]]
        class = "UnrelatedTest"
        decision = "accept"
        reason = "not a critical class"
        reviewer = "CTO"
        date = "2026-07-18"
        issue = "CON-199"
        """
    )
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py,
        exemptions=exemptions,
    )
    assert code == 1
    assert any("critical_class_globs" in p for p in report["problems"]), report["problems"]


# ── the denominator is the tree, not the mapping ─────────────────────────


def test_the_inventory_counts_inherited_and_non_Test_named_classes(tmp_path):
    """The upstream oracle does not follow the `*Test` naming convention, and
    most of its test methods are inherited from mixins. A scan that filters on
    either would silently understate the denominator, which is the number the
    whole floor is measured against.

    Concrete suite: 1 class named `*Test` with 1 direct method, 1 class not
    named `*Test` at all, and 1 mixin (subclassing `object`, as upstream's do)
    contributing an inherited method to both.
    """
    (tmp_path / "tests").mkdir(parents=True, exist_ok=True)
    (tmp_path / "tests" / "SomeMixin.py").write_text(
        "import unittest\n\n\n"
        "class SomeMixin(object):\n"
        "    def test_inherited(self):\n        pass\n"
    )
    (tmp_path / "tests" / "WeirdNames.py").write_text(
        "import unittest\n\n"
        "from SomeMixin import SomeMixin\n\n\n"
        "class NestedEnginePollTest(SomeMixin, unittest.TestCase):\n"
        "    def test_poll(self):\n        pass\n\n\n"
        "class NoTestSuffix(SomeMixin, unittest.TestCase):\n"
        "    pass\n\n\n"
        "class NotCollectedMixin(object):\n"
        "    def test_never_runs(self):\n        pass\n"
    )
    (tmp_path / "tests" / "RealTest.py").write_text(
        "import unittest\n\n"
        "from SomeMixin import SomeMixin\n\n\n"
        "class RealTest(SomeMixin, unittest.TestCase):\n"
        "    def test_direct(self):\n        pass\n"
    )

    sys.path.insert(0, str(HARNESS_DIR))
    import graduation_gate

    methods, _ = graduation_gate.scan_upstream_methods(tmp_path / "tests")
    got = {(m.cls, m.name) for m in methods}
    assert got == {
        ("RealTest", "test_direct"),
        ("RealTest", "test_inherited"),
        ("NestedEnginePollTest", "test_inherited"),
        ("NestedEnginePollTest", "test_poll"),
        ("NoTestSuffix", "test_inherited"),
    }, got


def test_a_method_absent_from_the_mapping_still_counts_against_coverage(tmp_path):
    """Dropping a mapping entry must not remove the method from the floor."""
    write_upstream({"UnrelatedTest": ["test_a", "test_b", "test_c", "test_d", "test_e"]}, tmp_path)
    mapping = '[upstream]\nrepo = "x"\n\n[UnrelatedTest]\ntest_a = ["m::tests::test_a"]\n'
    rust = rust_junit(["m::tests::test_a"])
    py = junit([("tests.UnrelatedTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=mapping, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    assert report["methods_total"] == 5
    assert report["methods_covered"] == 1
    assert report["verdict"] == "FAIL"


def test_a_mapping_to_a_nonexistent_rust_test_is_not_coverage(tmp_path):
    write_upstream({"TaskFooTest": ["test_a"]}, tmp_path)
    mapping = '[upstream]\nrepo = "x"\n\n[TaskFooTest]\ntest_a = ["task::tests::does_not_exist"]\n'
    rust = rust_junit(["task::tests::test_a"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=mapping, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    assert report["methods_covered"] == 0
    assert any("absent from the Rust-unit leg" in n for n in report["notes"])


def test_a_mapped_but_failing_rust_test_is_not_coverage(tmp_path):
    write_upstream({"TaskFooTest": ["test_a"]}, tmp_path)
    mapping = '[upstream]\nrepo = "x"\n\n[TaskFooTest]\ntest_a = ["task::tests::test_a"]\n'
    rust = rust_junit(["task::tests::test_a"], failures=["task::tests::test_a"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, report = run_gate(tmp_path, mapping=mapping, rust_unit=rust, python_original=py, python_rust=py)
    assert code == 1
    assert report["methods_covered"] == 0


# ── the one-line contract ────────────────────────────────────────────────


def test_the_verdict_is_the_first_line_and_carries_the_numbers(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    code, out, _ = run_gate(tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py)
    first = out.splitlines()[0]
    assert first.startswith("GRADUATION GATE: FAIL")
    assert "coverage=" in first and "required 80%" in first
    assert "covered=1/4" in first


# ── CI output channels ───────────────────────────────────────────────────


def test_the_gate_publishes_a_boolean_for_ci(tmp_path, monkeypatch):
    gh_out = tmp_path / "gh_output"
    gh_sum = tmp_path / "gh_summary"
    monkeypatch.setenv("GITHUB_OUTPUT", str(gh_out))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(gh_sum))

    py_classes = {**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]}
    write_upstream(py_classes, tmp_path)
    (tmp_path / "TEST_MAPPING.toml").write_text(MAPPED_ALL)
    (tmp_path / "results").mkdir(exist_ok=True)
    (tmp_path / "results" / "rust-unit.xml").write_text(
        rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                    "misc::tests::test_d", "misc::tests::test_e"])
    )
    py = py_all_pass(py_classes)
    (tmp_path / "results" / "python-original.xml").write_text(py)
    (tmp_path / "results" / "python-rust.xml").write_text(py)
    (tmp_path / "graduation-gate.toml").write_text(CONFIG)
    (tmp_path / "graduation-exemptions.toml").write_text(EXEMPTIONS_HEADER)

    proc = subprocess.run(
        [sys.executable, str(GATE),
         "--mapping", str(tmp_path / "TEST_MAPPING.toml"),
         "--tests-dir", str(tmp_path / "tests"),
         "--results-dir", str(tmp_path / "results"),
         "--config", str(tmp_path / "graduation-gate.toml"),
         "--exemptions", str(tmp_path / "graduation-exemptions.toml")],
        capture_output=True, text=True, cwd=REPO_ROOT,
    )
    assert proc.returncode == 0, proc.stdout

    published = dict(
        line.split("=", 1) for line in gh_out.read_text().splitlines() if "=" in line
    )
    assert published["graduation_gate"] == "PASS"
    assert published["covered"] == "5"
    assert "CON-4 graduation gate: PASS" in gh_sum.read_text()


def test_the_self_tests_do_not_write_to_the_ci_output_channels(tmp_path, monkeypatch):
    """These tests run in the same job, and therefore the same environment, as
    the real gate step. If they inherited GITHUB_OUTPUT the step would publish
    a `graduation_gate=` line per synthetic case, and the real signal would be
    indistinguishable from the noise around it."""
    gh_out = tmp_path / "gh_output"
    gh_sum = tmp_path / "gh_summary"
    gh_out.write_text("")
    gh_sum.write_text("")
    monkeypatch.setenv("GITHUB_OUTPUT", str(gh_out))
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(gh_sum))

    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a"])
    py = junit([("tests.TaskFooTest", "test_a", "pass")])
    run_gate(tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py)

    assert gh_out.read_text() == ""
    assert gh_sum.read_text() == ""


# ── unreadable legs ──────────────────────────────────────────────────────


def test_an_unreadable_leg_is_a_problem_not_an_empty_result(tmp_path):
    """A leg that could not be read must not look like a leg that ran and found
    nothing. Previously a missing python-original.xml left the regression check
    reporting a vacuous `[PASS] 0 method(s)`, and the run was still scored."""
    write_upstream(ALL_GREEN, tmp_path)
    (tmp_path / "TEST_MAPPING.toml").write_text(MAPPED_ALL)
    (tmp_path / "tests").mkdir(exist_ok=True)
    (tmp_path / "results").mkdir(exist_ok=True)
    # Only the Rust leg is present; both Python legs are absent entirely.
    (tmp_path / "results" / "rust-unit.xml").write_text(
        rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                    "misc::tests::test_d", "misc::tests::test_e"])
    )
    (tmp_path / "graduation-gate.toml").write_text(CONFIG)
    (tmp_path / "graduation-exemptions.toml").write_text(EXEMPTIONS_HEADER)

    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL,
        rust_unit=(tmp_path / "results" / "rust-unit.xml").read_text(),
        python_original=None, python_rust=None,
    )
    assert code == 1, out

    signal = next(c for c in report["checks"] if c["name"] == "trustworthy-signal")
    assert not signal["passed"], report["checks"]
    assert "2 unreadable leg(s)" in signal["measured"]
    assert any("python-original.xml" in p for p in report["problems"])

    # ...and the regression check is explicitly unmeasured rather than "0".
    reg = next(c for c in report["checks"] if c["name"] == "regressions")
    assert not reg["passed"]
    assert reg["measured"] == "not measured"

    # The failure must be printed, not merely recorded in the JSON.
    assert "missing" in out


def test_an_unparseable_leg_is_a_problem_not_an_empty_result(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    (tmp_path / "TEST_MAPPING.toml").write_text(MAPPED_ALL)
    (tmp_path / "tests").mkdir(exist_ok=True)
    (tmp_path / "results").mkdir(exist_ok=True)
    (tmp_path / "results" / "rust-unit.xml").write_text(
        rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                    "misc::tests::test_d", "misc::tests::test_e"])
    )
    # Valid XML, but not JUnit: must not be silently scored as an empty leg.
    (tmp_path / "results" / "python-original.xml").write_text(
        '<?xml version="1.0"?><notjunit><suite/></notjunit>'
    )
    (tmp_path / "results" / "python-rust.xml").write_text(
        '<?xml version="1.0"?><notjunit><suite/></notjunit>'
    )
    (tmp_path / "graduation-gate.toml").write_text(CONFIG)
    (tmp_path / "graduation-exemptions.toml").write_text(EXEMPTIONS_HEADER)

    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL,
        rust_unit=(tmp_path / "results" / "rust-unit.xml").read_text(),
        python_original=(tmp_path / "results" / "python-original.xml").read_text(),
        python_rust=(tmp_path / "results" / "python-rust.xml").read_text(),
    )
    assert code == 1, out
    signal = next(c for c in report["checks"] if c["name"] == "trustworthy-signal")
    assert not signal["passed"]


def test_a_measured_zero_regressions_is_still_reported_when_the_legs_are_readable(tmp_path):
    """The 'not measured' override must be tied to leg readability, not to the
    count. With both Python legs readable and green, the gate must be able to
    say 0 rather than refusing to answer."""
    write_upstream(ALL_GREEN, tmp_path)
    py_classes = {**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]}
    py = py_all_pass(py_classes)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                       "misc::tests::test_d", "misc::tests::test_e"])
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL, rust_unit=rust, python_original=py, python_rust=py
    )
    assert code == 0, out
    reg = next(c for c in report["checks"] if c["name"] == "regressions")
    assert reg["passed"]
    assert reg["measured"] == "0 method(s)"
    assert "regressions=0 " in out  # first token of the one-liner


# ── the coverage ladder ──────────────────────────────────────────────────


def test_the_ladder_locates_which_link_in_the_chain_is_short(tmp_path):
    """A single ratio over a three-way conjunction cannot say whether the gap is
    in the mapping, the Rust-port run, or the rust-backend run. Reporting
    "0.0%" alone makes the porting stream look untouched even when the Rust work
    is done and the chain is severed at the last leg."""
    py_classes = {**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]}
    write_upstream(py_classes, tmp_path)
    # Rust leg: everything green. rust-backend leg: nothing ran. Exactly the
    # CON-116 shape, where the mapping and the Rust work are both fine.
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                       "misc::tests::test_d", "misc::tests::test_e"])
    empty_py = junit([])
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL, rust_unit=rust,
        python_original=empty_py, python_rust=empty_py,
    )
    assert code == 1, out
    ladder = report["ladder"]
    assert ladder == {
        "upstream_test_cases": 5,
        "mapped": 5,
        "passing_rust_port_run": 5,
        "passing_both": 0,
        "min_method_coverage": 0.8,
    }
    assert "Coverage ladder:" in out
    assert "...and passing in the Rust-port run" in out
    # The line that tells the operator what to fix next.
    assert "5 method(s) pass in the Rust-port run but not in CONSORTIUM_BACKEND=rust" in out


def test_the_ladder_distinguishes_a_mapping_gap_from_a_run_gap(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    # MAPPED covers TaskFooTest.test_a/test_b and TreeBarTest.test_c, and
    # leaves UnrelatedTest.test_d unmapped. Nothing is unported mid-chain.
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c"])
    py = junit([("tests.TaskFooTest", "test_a", "pass"), ("tests.TaskFooTest", "test_b", "pass"),
                ("tests.TreeBarTest", "test_c", "pass")])
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED, rust_unit=rust, python_original=py, python_rust=py
    )
    ladder = report["ladder"]
    assert ladder["upstream_test_cases"] == 4
    assert ladder["mapped"] == 3
    assert ladder["passing_rust_port_run"] == 3
    assert ladder["passing_both"] == 3
    # Full chain satisfied for what is mapped, so no "fix this leg" advice.
    assert "that is the leg to fix next" not in out
    assert "fail in the Rust-port leg" not in out


def test_notes_are_printed_and_bounded(tmp_path):
    """Notes used to be computed, shipped in the JSON, and never printed -- so
    the most actionable fact the gate produces was invisible to anyone running
    it."""
    write_upstream({"BigTest": [f"test_{i:02d}" for i in range(40)]}, tmp_path)
    mapping = '[upstream]\nrepo = "x"\n\n[BigTest]\n' + "".join(
        f'test_{i:02d} = ["nope::tests::test_{i:02d}"]\n' for i in range(40)
    )
    py = junit([("tests.BigTest", f"test_{i:02d}", "pass") for i in range(40)])
    code, out, report = run_gate(
        tmp_path, mapping=mapping, rust_unit=rust_junit(["other::tests::x"]),
        python_original=py, python_rust=py,
    )
    assert code == 1
    assert "Notes (40 method-level findings):" in out
    assert "\u2026 20 more (see --json-out)" in out
    # The full set is always in the JSON, so nothing is lost by bounding stdout.
    assert len(report["notes"]) == 40


def test_note_wording_reads_as_english(tmp_path):
    write_upstream(ALL_GREEN, tmp_path)
    rust = rust_junit(["task::tests::test_a", "task::tests::test_b", "tree::tests::test_c",
                       "misc::tests::test_d", "misc::tests::test_e"])
    orig = py_all_pass({**ALL_GREEN, "UnrelatedTest": ["test_d", "test_e"]})
    rust_py = orig.replace(
        '<testcase classname="tests.MsgTreeTest" name="test_001_basics"/>',
        '<testcase classname="tests.MsgTreeTest" name="test_001_basics"><failure message="x"/></testcase>',
    )
    if rust_py == orig:  # MsgTreeTest is not in this fixture; use TaskFooTest
        rust_py = orig.replace(
            '<testcase classname="tests.TaskFooTest" name="test_b"/>',
            '<testcase classname="tests.TaskFooTest" name="test_b"><error message="x"/></testcase>',
        )
    code, out, report = run_gate(
        tmp_path, mapping=MAPPED_ALL, rust_unit=rust,
        python_original=orig, python_rust=rust_py,
    )
    assert code == 1
    joined = "\n".join(report["notes"])
    assert " failed in the CONSORTIUM_BACKEND=rust run" in joined or \
           " errored in the CONSORTIUM_BACKEND=rust run" in joined
    assert "but fail in" not in joined
