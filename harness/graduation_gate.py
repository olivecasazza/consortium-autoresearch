#!/usr/bin/env python3
"""graduation_gate.py — the CON-4 graduation gate.

One command, one verdict: is the Rust feature set verified against the upstream
ClusterShell tests well enough that the side-by-side oracle can be deleted?

    python harness/graduation_gate.py

Exits 0 and prints `GRADUATION GATE: PASS` when the floor is met, exits 1 and
prints `GRADUATION GATE: FAIL` with the measured-vs-required numbers when it is
not. Read-only and idempotent: it measures the tree and the JUnit XML in
`results/`, and writes nothing outside `--json-out`.

Criteria, and where each is configured in harness/graduation-gate.toml:

  1. Coverage       >= `min_method_coverage` of upstream test methods are
                    simultaneously mapped in TEST_MAPPING.toml, passing in the
                    Rust-port run, and passing in the CONSORTIUM_BACKEND=rust
                    Python run.
  1b. No regressions a method that passes under CONSORTIUM_BACKEND=python but
                    not under CONSORTIUM_BACKEND=rust fails the gate. No
                    tolerance.
  2. Critical paths every class matching `critical_class_globs` is fully
                    covered, or carries a reviewed decision in
                    harness/graduation-exemptions.toml.
  3. Trustworthy    the Rust-unit leg is non-empty and neither Python leg has a
      signal         collection error. CON-198 made the Rust leg measure
                    consortium-py instead of consortium-crate; it produced a
                    `tests="0"` JUnit file and looked green. An empty or
                    errored leg is a FAIL, never a PASS.

The floor, its approver, and what retiring the harness would delete are in
docs/graduation-gate.md.
"""

from __future__ import annotations

import argparse
import ast
import fnmatch
import json
import os
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ModuleNotFoundError:
        sys.exit("graduation_gate.py needs Python 3.11+ (tomllib), or `tomli` installed")

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_CONFIG = Path("harness/graduation-gate.toml")
DEFAULT_EXEMPTIONS = Path("harness/graduation-exemptions.toml")
DEFAULT_RESULTS = Path("results")

EXEMPTION_REQUIRED_FIELDS = ("class", "decision", "reason", "reviewer", "date", "issue")
VALID_DECISIONS = ("accept", "defer")


# ── upstream inventory ────────────────────────────────────────────────────


@dataclass
class Method:
    cls: str
    name: str

    @property
    def key(self) -> str:
        return f"{self.cls}.{self.name}"


def scan_upstream_methods(tests_dir: Path) -> tuple[list[Method], list[str]]:
    """Authoritative inventory of upstream test cases, straight from the AST.

    The denominator of the coverage ratio comes from here, not from
    TEST_MAPPING.toml, so a method cannot be removed from the denominator by
    dropping its mapping entry.

    Two things this deliberately does *not* do, both of which understate the
    denominator:

    - Filter classes by a naming convention. `harness/generate_test_mapping.py`
      keeps only classes whose name ends in `Test`, but upstream ClusterShell
      does not follow that: `CLIClushTest_A`, `CLIClubakTestGroupsConf`,
      `CLINodesetGroupResolverTest1`, `TaskLocalEnginePollTest` and 11 others
      are all real test cases pytest runs, and 220 of the 1075 test cases live
      in classes that filter drops. The gate counts what the oracle executes.
    - Only count methods written directly in the class body. Test methods are
      inherited from mixins (`TaskLocalMixin`, `TaskDistantMixin`,
      `TaskDistantPdshMixin`), so a direct-definition scan misses 490 of the
      1075 cases pytest runs.

    Mixin base classes are excluded from the result, because pytest does not
    collect them either — it collects their concrete subclasses.
    """
    warnings: list[str] = []
    bases: dict[str, list[str]] = {}
    own: dict[str, set[str]] = {}

    def base_name(node: ast.expr) -> str | None:
        if isinstance(node, ast.Name):
            return node.id
        if isinstance(node, ast.Attribute):
            return node.attr
        return None

    for test_file in sorted(tests_dir.glob("*.py")):
        if test_file.name == "__init__.py":
            continue
        try:
            tree = ast.parse(test_file.read_text(), filename=str(test_file))
        except SyntaxError as exc:
            warnings.append(f"unparseable {test_file.name}: {exc}")
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.ClassDef):
                continue
            bases[node.name] = [n for n in (base_name(b) for b in node.bases) if n]
            own[node.name] = {
                item.name
                for item in node.body
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef))
                and item.name.startswith("test")
            }

    def resolve(name: str, seen: frozenset[str] = frozenset()) -> set[str]:
        if name in seen or name not in own:
            return set()
        out = set(own[name])
        for base in bases.get(name, []):
            out |= resolve(base, seen | {name})
        return out

    def is_test_case(name: str, seen: frozenset[str] = frozenset()) -> bool:
        if name in seen:
            return False
        for base in bases.get(name, []):
            if base in ("TestCase", "IsolatedAsyncioTestCase") or is_test_case(base, seen | {name}):
                return True
        return False

    effective = {cls: resolve(cls) for cls in own}

    methods: list[Method] = []
    for cls in sorted(effective):
        # A test case is a class that reaches unittest.TestCase and defines or
        # inherits at least one test method. The upstream mixins
        # (TaskLocalMixin, TaskDistantMixin, TaskDistantPdshMixin) subclass
        # `object`, not TestCase, so they are correctly excluded — pytest does
        # not collect them either, only their concrete subclasses.
        if not effective[cls] or not is_test_case(cls):
            continue
        methods.extend(Method(cls, name) for name in sorted(effective[cls]))

    return methods, warnings


# ── JUnit ─────────────────────────────────────────────────────────────────


def parse_junit(path: Path) -> tuple[dict[str, str], int, list[str]]:
    """-> ({test_key: status}, collection_error_count, unparsed_notes)."""
    if not path.exists():
        return {}, 0, [f"missing {path}"]

    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as exc:
        return {}, 0, [f"unparseable {path}: {exc}"]

    results: dict[str, str] = {}
    collection_errors = 0
    suites = root.findall(".//testsuite")
    if root.tag == "testsuite":
        suites = [root]

    for suite in suites:
        for tc in suite.findall("testcase"):
            classname = (tc.get("classname") or "").strip()
            name = (tc.get("name") or "").strip()
            if not classname:
                # pytest emits a module-level <testcase classname=""> holding a
                # collection error. It is not a test method.
                if tc.find("error") is not None or tc.find("failure") is not None:
                    collection_errors += 1
                continue
            # `tests.RangeSetTest.test_x` and `RangeSetTest.test_x` both land on
            # the same key, so normalize away any dotted module prefix.
            module = classname.rsplit(".", 1)[-1]
            full = f"{module}.{name}"
            if tc.find("failure") is not None:
                results[full] = "fail"
            elif tc.find("error") is not None:
                results[full] = "error"
            elif tc.find("skipped") is not None:
                results[full] = "skip"
            else:
                results[full] = "pass"

    return results, collection_errors, []


def rust_test_index(results: dict[str, str]) -> dict[str, str]:
    """Index Rust-unit results by their raw test name.

    cargo_to_junit.py writes classname="consortium", so the parsed keys are
    `consortium.range_set::tests::test_x`, while TEST_MAPPING.toml says
    `range_set::tests::test_x`. Index both spellings.
    """
    index: dict[str, str] = {}
    for key, status in results.items():
        index[key] = status
        if key.startswith("consortium."):
            index[key.removeprefix("consortium.")] = status
    return index


# ── config ────────────────────────────────────────────────────────────────


@dataclass
class Config:
    min_method_coverage: float
    critical_class_globs: list[str]
    require_nonempty_rust_leg: bool
    require_collection_error_free: bool
    rust_leg_crate: str
    upstream_ref: str
    approved_by: str
    approved_on: str
    issue: str
    warnings: list[str] = field(default_factory=list)


def load_config(path: Path) -> Config:
    data = tomllib.loads(path.read_text())
    warnings: list[str] = []
    if path == DEFAULT_CONFIG:
        warnings.append(f"using non-default config {path}")

    ref_file = data.get("upstream", {}).get("ref_file", "UPSTREAM_REF")
    ref_path = REPO_ROOT / ref_file
    upstream_ref = ref_path.read_text().strip() if ref_path.exists() else "unknown"

    floor = data.get("floor", {})
    return Config(
        min_method_coverage=float(data["min_method_coverage"]),
        critical_class_globs=list(data["critical_class_globs"]),
        require_nonempty_rust_leg=bool(data.get("require_nonempty_rust_leg", True)),
        require_collection_error_free=bool(data.get("require_collection_error_free", True)),
        rust_leg_crate=data.get("rust_leg_crate", "consortium-crate"),
        upstream_ref=upstream_ref,
        approved_by=floor.get("approved_by", "unknown"),
        approved_on=floor.get("approved_on", "unknown"),
        issue=floor.get("issue", "unknown"),
        warnings=warnings,
    )


@dataclass
class Exemption:
    cls: str
    decision: str
    reason: str
    reviewer: str
    date: str
    issue: str


def load_exemptions(path: Path, globs: list[str]) -> tuple[dict[str, Exemption], list[str]]:
    """-> ({class: Exemption}, problems).

    A malformed entry is a hard problem, not a warning: an unreviewed decision
    must not silently satisfy criterion 2.
    """
    if not path.exists():
        return {}, [f"missing {path}"]

    raw = tomllib.loads(path.read_text()).get("exemption", [])
    out: dict[str, Exemption] = {}
    problems: list[str] = []

    for i, entry in enumerate(raw):
        label = entry.get("class") or f"entry #{i}"
        missing = [f for f in EXEMPTION_REQUIRED_FIELDS if not str(entry.get(f, "")).strip()]
        if missing:
            problems.append(f"exemption {label}: missing {', '.join(missing)}")
            continue
        if entry["decision"] not in VALID_DECISIONS:
            problems.append(
                f"exemption {label}: decision {entry['decision']!r} not in {VALID_DECISIONS}"
            )
            continue
        if not any(fnmatch.fnmatchcase(entry["class"], g) for g in globs):
            problems.append(
                f"exemption {label}: does not match any critical_class_globs {globs}; "
                "remove it or widen the globs"
            )
            continue
        out[entry["class"]] = Exemption(
            cls=entry["class"],
            decision=entry["decision"],
            reason=entry["reason"],
            reviewer=entry["reviewer"],
            date=entry["date"],
            issue=entry["issue"],
        )

    return out, problems


# ── the gate ──────────────────────────────────────────────────────────────


@dataclass
class Check:
    name: str
    passed: bool
    measured: str
    required: str
    detail: str = ""


def evaluate(
    methods: list[Method],
    mapping: dict[str, dict[str, list[str]]],
    py_orig: dict[str, str],
    py_rust: dict[str, str],
    rust_unit: dict[str, str],
    cfg: Config,
    exemptions: dict[str, Exemption],
    collection_errors: dict[str, int],
    notes: list[str],
) -> tuple[list[Check], dict[str, dict]]:
    checks: list[Check] = []
    rust_idx = rust_test_index(rust_unit)

    per_class: dict[str, dict] = {}
    for m in methods:
        targets = mapping.get(m.cls, {}).get(m.name, [])
        is_mapped = bool(targets)
        # A method is passing in the Rust-port run only if every Rust test it
        # maps to exists and passed. A mapping to a test that does not exist is
        # not coverage.
        rust_statuses = [rust_idx.get(t) for t in targets]
        rust_ok = is_mapped and all(s == "pass" for s in rust_statuses)
        missing_targets = [t for t, s in zip(targets, rust_statuses) if s is None]
        py_rust_status = py_rust.get(m.key)
        py_orig_status = py_orig.get(m.key)
        covered = rust_ok and py_rust_status == "pass"

        row = per_class.setdefault(m.cls, {"total": 0, "covered": 0, "mapped": 0})
        row["total"] += 1
        row["mapped"] += int(is_mapped)
        row["covered"] += int(covered)

        if missing_targets:
            notes.append(
                f"{m.key}: mapped to {len(missing_targets)} Rust test(s) absent from the "
                f"Rust-unit leg (e.g. {missing_targets[0]}) — not counted as coverage"
            )
        if is_mapped and py_rust_status is None:
            notes.append(f"{m.key}: mapped but absent from the CONSORTIUM_BACKEND=rust run")
        elif py_rust_status is not None and py_rust_status != "pass":
            notes.append(f"{m.key}: mapped but {py_rust_status} in the CONSORTIUM_BACKEND=rust run")

    total = len(methods)
    covered = sum(r["covered"] for r in per_class.values())
    mapped = sum(r["mapped"] for r in per_class.values())
    coverage = (covered / total) if total else 0.0

    # 1. coverage
    checks.append(
        Check(
            name="coverage",
            passed=coverage >= cfg.min_method_coverage,
            measured=f"{coverage * 100:.1f}% ({covered}/{total} methods)",
            required=f">= {cfg.min_method_coverage * 100:.1f}%",
            detail="mapped in TEST_MAPPING.toml AND passing in the Rust-port run AND "
            "passing in the CONSORTIUM_BACKEND=rust run",
        )
    )

    # 1b. no regressions vs the CONSORTIUM_BACKEND=python baseline
    regressions = [
        m.key
        for m in methods
        if py_orig.get(m.key) == "pass" and py_rust.get(m.key) in ("fail", "error")
    ]
    checks.append(
        Check(
            name="regressions",
            passed=not regressions,
            measured=f"{len(regressions)} method(s)",
            required="0",
            detail=", ".join(regressions[:5]) + (" …" if len(regressions) > 5 else ""),
        )
    )

    # 2. no unmapped critical paths
    critical = sorted(c for c in per_class if any(fnmatch.fnmatchcase(c, g) for g in cfg.critical_class_globs))
    gaps: list[str] = []
    for cls in critical:
        row = per_class[cls]
        if row["covered"] == row["total"]:
            continue
        if cls in exemptions:
            continue
        gaps.append(f"{cls} {row['covered']}/{row['total']}")
    checks.append(
        Check(
            name="critical-paths",
            passed=not gaps,
            measured=f"{len(critical) - len(gaps)}/{len(critical)} critical classes fully covered",
            required=f"{len(critical)}/{len(critical)} (or a reviewed decision)",
            detail=", ".join(gaps[:6]) + (" …" if len(gaps) > 6 else ""),
        )
    )

    # 3. the signal itself is trustworthy
    signal_problems: list[str] = []
    if cfg.require_nonempty_rust_leg and not rust_unit:
        signal_problems.append(
            f"the Rust-unit leg produced no results — it is not measuring {cfg.rust_leg_crate}"
        )
    if cfg.require_collection_error_free:
        for leg, count in sorted(collection_errors.items()):
            if count:
                signal_problems.append(f"{leg} had {count} collection error(s) — the run is partial")
    checks.append(
        Check(
            name="trustworthy-signal",
            passed=not signal_problems,
            measured=f"{len(rust_unit)} Rust-unit results, "
            f"{sum(collection_errors.values())} collection error(s)",
            required="a non-empty Rust-unit leg and error-free Python collection",
            detail="; ".join(signal_problems),
        )
    )

    return checks, per_class


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--results-dir", default=DEFAULT_RESULTS, type=Path)
    ap.add_argument("--mapping", default=Path("TEST_MAPPING.toml"), type=Path)
    ap.add_argument("--config", default=DEFAULT_CONFIG, type=Path)
    ap.add_argument("--exemptions", default=DEFAULT_EXEMPTIONS, type=Path)
    ap.add_argument("--tests-dir", default=Path("tests"), type=Path)
    ap.add_argument("--json-out", type=Path, help="also write the full report as JSON")
    args = ap.parse_args()

    def repo_path(p: Path) -> Path:
        return p if p.is_absolute() else REPO_ROOT / p

    config_path, exemptions_path = repo_path(args.config), repo_path(args.exemptions)
    results_dir = repo_path(args.results_dir)
    mapping_path, tests_dir = repo_path(args.mapping), repo_path(args.tests_dir)

    notes: list[str] = []
    problems: list[str] = []

    if not config_path.exists():
        sys.exit(f"graduation_gate.py: missing config {config_path}")
    cfg = load_config(config_path)
    notes.extend(cfg.warnings)

    if not mapping_path.exists():
        sys.exit(f"graduation_gate.py: missing mapping {mapping_path}")
    raw_mapping = tomllib.loads(mapping_path.read_text())
    mapping = {
        k: v for k, v in raw_mapping.items() if isinstance(v, dict) and k not in ("upstream", "_extra_rust_tests")
    }

    exemptions, ex_problems = load_exemptions(exemptions_path, cfg.critical_class_globs)
    problems.extend(ex_problems)

    methods, scan_warnings = scan_upstream_methods(tests_dir)
    notes.extend(scan_warnings)
    if not methods:
        sys.exit(f"graduation_gate.py: found no upstream test methods under {tests_dir}")

    py_orig, orig_ce, orig_notes = parse_junit(results_dir / "python-original.xml")
    py_rust, rust_ce, rust_notes = parse_junit(results_dir / "python-rust.xml")
    rust_unit, _, unit_notes = parse_junit(results_dir / "rust-unit.xml")
    notes.extend(orig_notes + rust_notes + unit_notes)

    checks, per_class = evaluate(
        methods, mapping, py_orig, py_rust, rust_unit, cfg, exemptions,
        {"CONSORTIUM_BACKEND=python": orig_ce, "CONSORTIUM_BACKEND=rust": rust_ce},
        notes,
    )
    problems.extend(n for n in notes if n.startswith("graduation_gate:"))

    passed = all(c.passed for c in checks) and not problems
    covered = sum(r["covered"] for r in per_class.values())

    coverage_check = next(c for c in checks if c.name == "coverage")
    verdict = "PASS" if passed else "FAIL"
    print(
        f"GRADUATION GATE: {verdict}  "
        f"coverage={coverage_check.measured.split(' ', 1)[0]} "
        f"(required {cfg.min_method_coverage * 100:.0f}%)  "
        f"covered={covered}/{len(methods)}  "
        f"regressions={next(c for c in checks if c.name == 'regressions').measured.split(' ', 1)[0]}  "
        f"critical_classes_gap={next(c for c in checks if c.name == 'critical-paths').measured.split(' ', 1)[0]}"
    )
    print()

    for c in checks:
        print(f"  [{'PASS' if c.passed else 'FAIL'}] {c.name}")
        print(f"         measured: {c.measured}")
        print(f"         required: {c.required}")
        if c.detail:
            print(f"         detail:   {c.detail}")
    print()

    if problems:
        print("Problems:")
        for p in problems:
            print(f"  - {p}")
        print()

    print(
        f"floor {cfg.min_method_coverage * 100:.0f}% approved by {cfg.approved_by} "
        f"on {cfg.approved_on} ({cfg.issue}); upstream {cfg.upstream_ref}; "
        f"Rust leg {cfg.rust_leg_crate}"
    )
    if exemptions:
        print(f"reviewed decisions in force: {', '.join(sorted(exemptions))}")
    print("see docs/graduation-gate.md for what retiring the side-by-side harness would delete")

    if args.json_out:
        out = repo_path(args.json_out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(
            json.dumps(
                {
                    "verdict": verdict,
                    "passed": passed,
                    "upstream_ref": cfg.upstream_ref,
                    "rust_leg_crate": cfg.rust_leg_crate,
                    "min_method_coverage": cfg.min_method_coverage,
                    "approved_by": cfg.approved_by,
                    "approved_on": cfg.approved_on,
                    "issue": cfg.issue,
                    "methods_total": len(methods),
                    "methods_covered": covered,
                    "checks": [
                        {
                            "name": c.name,
                            "passed": c.passed,
                            "measured": c.measured,
                            "required": c.required,
                            "detail": c.detail,
                        }
                        for c in checks
                    ],
                    "exemptions": {k: vars(v) for k, v in exemptions.items()},
                    "per_class": per_class,
                    "notes": notes,
                    "problems": problems,
                },
                indent=2,
            )
            + "\n"
        )
        print(f"wrote {out}")

    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as fh:
            fh.write(f"graduation_gate={verdict}\n")
            fh.write(f"covered={covered}\n")
            fh.write(f"methods_total={len(methods)}\n")
            fh.write(f"coverage={coverage_check.measured.split(' ', 1)[0]}\n")

    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as fh:
            fh.write(f"## CON-4 graduation gate: {verdict}\n\n")
            fh.write(
                f"`{coverage_check.measured}` of upstream test methods covered; "
                f"floor `{cfg.min_method_coverage * 100:.0f}%`.\n\n"
            )
            fh.write("| check | verdict | measured | required |\n|---|---|---|---|\n")
            for c in checks:
                fh.write(f"| {c.name} | {'PASS' if c.passed else 'FAIL'} | {c.measured} | {c.required} |\n")
            fh.write("\n")

    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
