#!/usr/bin/env python3
"""check_test_gates.py — fail if a CI test step cannot go red.

Until CON-97 landed, every test-producing step in this repo was wrapped in
`|| true` or `continue-on-error: true`, so `consortium` CI could not go red on
a failing test and no measurement in the migration program produced a signal.

This is the regression guard for that fix. It is deliberately dependency-free
and deliberately dumb: it does not try to understand YAML. It requires that
every swallow token in `.github/workflows/` carries an explicit justification
marker on the preceding line, so re-adding a guard is a loud, reviewable act
rather than a silent one.

It also enforces the second, quieter form of the same defect. GitHub's default
step condition is `success()`, so a test step that follows another test step in
the same job is *skipped* — not red — as soon as its sibling is red. A skipped
test step produces no signal, which is the same failure mode as a swallowed one,
and it is invisible in the job conclusion. Every test step after the first in a
job must therefore carry an `if:` that survives a sibling failure
(`!cancelled()` or `always()`).

And it enforces the third, loudest form (CON-209): a `pull_request` or
`merge_group` trigger restricted by a `branches:` allowlist runs *no* CI at all
for a PR aimed at any other base, and reports nothing. This repo stacks PRs, so
an allowlist naming only `main`/`master`/`develop` meant every PR stacked on
`ci/un-swallow-gates` rendered as "no checks reported" — indistinguishable from
"no problems" without a deliberate `gh pr checks`. A `branches:` filter on a PR
or merge-queue trigger must therefore carry a `justified-trigger:` comment.

Usage:
    python3 harness/ci/check_test_gates.py [--all] [workflow-dir]
    python3 harness/ci/check_test_gates.py --self-test

    (default)  audit the workflows CON-97 un-swallowed: ci.yml and
               migration-scorecard.yml
    --all      audit every workflow in the directory, including
               nosetests.yml, which is still fully swallowed upstream
    --self-test  run the audits against built-in fixtures that must be
               rejected and fixtures that must pass, so the gate is proven able
               to go red rather than merely believed to

Exit codes:
    0  every swallow token is justified (or there are none), every non-first
       test step in a job is reachable after a sibling failure, every PR
       trigger is unfiltered, and every self-test fixture behaved as specified
    1  at least one unjustified swallow token, a test step that can only ever
       be skipped, a branch-filtered PR trigger, or a self-test fixture that
       did not behave as specified
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW_DIR = REPO_ROOT / ".github" / "workflows"

# Tokens that make a step's failure non-fatal for the job.
GUARD_PATTERNS = (
    re.compile(r"\|\|\s*true\b"),
    re.compile(r"^\s*continue-on-error:\s*true\s*$"),
)

# A swallow token is only tolerated when a comment block carrying this marker
# governs it. A marked block justifies everything from the end of the block up
# to the start of the next comment block or the next step header, so a guard can
# be justified either as a whole step or as one line inside a `run:` block.
JUSTIFICATION_MARKER = "justified-guard:"

# CON-209: the same "loud and deliberate, never silent" rule applied to trigger
# filters. A `branches:` allowlist on a PR/merge-queue trigger is tolerated only
# when a comment block carrying this marker governs it.
TRIGGER_MARKER = "justified-trigger:"

COMMENT_LINE = re.compile(r"^\s*#")


def rel(path: Path) -> Path:
    try:
        return path.relative_to(REPO_ROOT)
    except ValueError:
        return path

# How many non-comment lines may sit between a guard and its justification.
# 1 covers `continue-on-error:` sitting under its `- name:` header; 1 more covers
# a guard written as the last line of a `run:` block. Anything further away is
# treated as unjustified on purpose: the reason must be next to the guard.
MAX_JUSTIFICATION_DISTANCE = 3

WORKFLOW_GLOBS = ("*.yml", "*.yaml")

# A step whose name reads as "Run <something> tests" is treated as a test step.
# This is the whole vocabulary used by ci.yml and migration-scorecard.yml, and it
# deliberately excludes publish/upload/check/verify steps, which are not test
# producers.
TEST_STEP = re.compile(r"^run\b.*\btests?\b", re.IGNORECASE)

# Conditions that keep a step reachable after an earlier step failed.
SURVIVES_FAILURE = re.compile(r"!\s*cancelled\(\)|^\s*if:\s*always\(\)", re.IGNORECASE)

JOB_HEADER = re.compile(r"^ {2}([A-Za-z0-9_-]+):\s*$")
STEP_HEADER = re.compile(r"^ {6}- (name|uses|run|id):\s*(.*)$")
STEP_NAME = re.compile(r"^ {8}name:\s*(.*)$")
STEP_IF = re.compile(r"^ {8}if:\s*(.*)$")
CONTINUE_ON_ERROR = re.compile(r"^ {8}continue-on-error:\s*true\s*$")

# Audited by default. `nosetests.yml` is the upstream ClusterShell mirror and is
# still swallowed end to end; it is tracked by a separate follow-up rather than
# being quietly folded into the CON-97 change.
DEFAULT_WORKFLOWS = ("ci.yml", "migration-scorecard.yml")

# ── Trigger coverage (CON-209) ────────────────────────────────────────
# Triggers whose scope decides whether a *proposed change* is tested at all.
# `push` is deliberately absent: filtering pushes to the default branches is
# correct and cheap, and a PR event already covers the same code.
PR_TRIGGERS = ("pull_request", "merge_group")

ON_BLOCK = re.compile(r"^(?:\"|')?on(?:\"|')?:\s*(\S.*)?$")
TRIGGER_HEADER = re.compile(r"^ {2}([A-Za-z0-9_-]+):\s*(\S.*)?$")
BRANCHES_FILTER = re.compile(r"^ {4}branches:")


def is_justified(lines: list[str], index: int, marker: str = JUSTIFICATION_MARKER) -> bool:
    """True when the nearest comment block above `index` carries the marker."""
    for distance in range(1, MAX_JUSTIFICATION_DISTANCE + 1):
        candidate = index - distance
        if candidate < 0:
            return False
        text = lines[candidate]
        if not text.strip():
            continue
        if not COMMENT_LINE.match(text):
            continue
        # Walk the whole contiguous comment block the line belongs to.
        start = candidate
        while start > 0 and COMMENT_LINE.match(lines[start - 1]):
            start -= 1
        block = lines[start : candidate + 1]
        return any(marker in entry for entry in block)
    return False


def audit(path: Path) -> list[str]:
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    for number, line in enumerate(lines, start=1):
        if COMMENT_LINE.match(line):
            # A comment cannot swallow a failure; only quote the token.
            continue
        if not any(pattern.search(line) for pattern in GUARD_PATTERNS):
            continue
        if is_justified(lines, number - 1):
            continue
        problems.append(f"{rel(path)}:{number}: unguarded failure swallow -> {line.strip()}")
    return problems


def iter_test_steps(lines: list[str]):
    """Yield (job, line_number, name, survives_failure) per test step in a job.

    Deliberately dumb indentation parsing, matching the file's existing
    "does not try to understand YAML" stance: it only reads step headers and the
    few keys it needs, and it never evaluates expressions.
    """
    job = None
    in_jobs = False
    seen_in_job = 0
    index = 0
    total = len(lines)
    while index < total:
        line = lines[index]
        if line.rstrip() == "jobs:":
            in_jobs = True
        elif in_jobs and not line.startswith(" ") and line.strip():
            in_jobs = False
        elif in_jobs:
            header = JOB_HEADER.match(line)
            if header:
                job, seen_in_job = header.group(1), 0
            elif STEP_HEADER.match(line):
                header = STEP_HEADER.match(line)
                name = header.group(2) if header.group(1) == "name" else None
                condition = None
                tolerated = False
                # Read forward to the next step header or job header.
                probe = index
                while probe + 1 < total and not STEP_HEADER.match(lines[probe + 1]):
                    probe += 1
                    nxt = lines[probe]
                    if JOB_HEADER.match(nxt):
                        probe -= 1
                        break
                    found = STEP_NAME.match(nxt)
                    if found and name is None:
                        name = found.group(1).strip()
                    found = STEP_IF.match(nxt)
                    if found:
                        condition = found.group(1).strip()
                    if CONTINUE_ON_ERROR.match(nxt):
                        tolerated = True
                if name and TEST_STEP.match(name) and not tolerated:
                    seen_in_job += 1
                    yield (
                        job,
                        index + 1,
                        name,
                        seen_in_job == 1,
                        bool(condition and SURVIVES_FAILURE.search(condition)),
                    )
                index = probe
        index += 1


def audit_unreachable_test_steps(path: Path) -> list[str]:
    """Flag test steps that GitHub would skip whenever a sibling test step fails."""
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    for job, number, name, is_first, survives in iter_test_steps(lines):
        if is_first or survives:
            continue
        problems.append(
            f"{rel(path)}:{number}: test step can only be skipped, never red "
            f"[{name}] — add `if: ${{{{ !cancelled() }}}}` so it still runs when an "
            f"earlier test step in job '{job}' fails"
        )
    return problems


def iter_branch_filtered_triggers(path: Path):
    """Yield (line_number, trigger) for every PR trigger narrowed by `branches:`.

    Deliberately dumb, like the rest of this file. It reads the top-level `on:`
    block, notes which events it declares, and reports any PR-facing event whose
    body carries a `branches:` allowlist. The block ends at the first
    non-comment line that is not indented, so `permissions:` and `jobs:` cannot
    be mistaken for triggers.
    """
    lines = path.read_text(encoding="utf-8").splitlines()
    in_on = False
    current = None
    for index, line in enumerate(lines):
        if not in_on:
            match = ON_BLOCK.match(line)
            if match:
                in_on = True
                # `on: [push]` / `on: push` names its events inline; there is no
                # block to inspect and nothing can be filtered.
            continue
        if not line.strip() or COMMENT_LINE.match(line):
            continue
        if not line.startswith(" "):
            in_on = False
            current = None
            continue
        header = TRIGGER_HEADER.match(line)
        if header:
            current = header.group(1)
            continue
        if BRANCHES_FILTER.match(line) and current in PR_TRIGGERS:
            yield index + 1, current


def audit_trigger_coverage(path: Path) -> list[str]:
    """Flag PR triggers that skip CI entirely for non-default base branches.

    A `branches:` allowlist on `pull_request`/`merge_group` does not make a check
    pass quietly — it makes the check *not exist* for any PR outside the list,
    and GitHub renders that absence as "no checks reported". This repo stacks
    PRs onto integration branches, so any allowlist is a live hole unless it is
    justified in review.
    """
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    for number, trigger in iter_branch_filtered_triggers(path):
        if is_justified(lines, number - 1, TRIGGER_MARKER):
            continue
        problems.append(
            f"{rel(path)}:{number}: {trigger} is restricted by a `branches:` filter — "
            f"a PR aimed at any other base branch runs no CI here and reports "
            f"`no checks reported`. Drop the filter, or record a deliberate "
            f"exception as a `{TRIGGER_MARKER}` comment above it."
        )
    return problems


# ── Self-test ─────────────────────────────────────────────────────────
# A gate nobody has ever seen fail is a gate nobody can trust. CON-97 already
# established that standard in this repo ("a test step that can only be skipped
# is as blind as a swallowed one"), so each audit below is paired with a fixture
# that MUST be rejected and one that MUST pass. If a future edit quietly turns
# an audit into a no-op, `--self-test` goes red instead of the repo quietly
# losing its guard.
#
# Fixtures are written to disk rather than passed as strings so the self-test
# exercises the same read-the-file-and-parse-indentation path CI uses.
SELF_TEST_CASES: tuple[tuple[str, str, bool], ...] = (
    (
        "unjustified-swallow.yml",
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run unit tests\n"
        "        run: cargo test --workspace || true\n",
        True,
    ),
    (
        "justified-swallow.yml",
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Collect-only probe\n"
        "        # justified-guard: collects targets, runs nothing\n"
        "        run: cargo test --workspace --no-run || true\n",
        False,
    ),
    (
        "unreachable-test-step.yml",
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run rust tests\n"
        "        run: cargo test --workspace\n"
        "      - name: Run integration tests\n"
        "        run: cargo test --workspace --test integration\n",
        True,
    ),
    (
        "surviving-test-step.yml",
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run rust tests\n"
        "        run: cargo test --workspace\n"
        "      - name: Run integration tests\n"
        "        if: ${{ !cancelled() }}\n"
        "        run: cargo test --workspace --test integration\n",
        False,
    ),
    (
        "branch-filtered-pr.yml",
        "on:\n"
        "  push:\n"
        "    branches: [main, master]\n"
        "  pull_request:\n"
        "    branches: [main, master]\n"
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run unit tests\n"
        "        run: cargo test --workspace\n",
        True,
    ),
    (
        "justified-branch-filtered-pr.yml",
        "on:\n"
        "  push:\n"
        "    branches: [main, master]\n"
        "  # justified-trigger: mirrors run only on release branches\n"
        "  pull_request:\n"
        "    branches: [main, master]\n"
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run unit tests\n"
        "        run: cargo test --workspace\n",
        False,
    ),
    (
        "push-filtered-is-fine.yml",
        "on:\n"
        "  push:\n"
        "    branches: [main, master]\n"
        "  pull_request:\n"
        "  merge_group:\n"
        "jobs:\n"
        "  unit:\n"
        "    steps:\n"
        "      - name: Run unit tests\n"
        "        run: cargo test --workspace\n",
        False,
    ),
)

AUDITS = (audit, audit_unreachable_test_steps, audit_trigger_coverage)


def self_test() -> int:
    import tempfile

    failures: list[str] = []
    with tempfile.TemporaryDirectory() as raw:
        directory = Path(raw)
        for name, body, must_flag in SELF_TEST_CASES:
            path = directory / name
            path.write_text(body, encoding="utf-8")
            flagged = any(check(path) for check in AUDITS)
            if flagged is not must_flag:
                verdict = "was flagged" if flagged else "was accepted"
                expectation = "rejected" if must_flag else "accepted"
                failures.append(f"{name}: {verdict}, but the self-test expects it to be {expectation}")
    if failures:
        print("::error::check_test_gates self-test failed — a guard is not load-bearing.")
        for failure in failures:
            print(f"::error::{failure}")
        return 1
    print(f"OK: self-test — {len(SELF_TEST_CASES)} fixtures, every audit able to fail and to pass.")
    return 0


def main(argv: list[str]) -> int:
    if "--self-test" in argv[1:]:
        return self_test()
    args = [a for a in argv[1:] if a != "--all"]
    sweep_all = "--all" in argv[1:]
    target = Path(args[0]) if args else WORKFLOW_DIR
    if not target.is_dir():
        print(f"::error::workflow directory not found: {target}")
        return 1

    if sweep_all:
        files = sorted({p for pattern in WORKFLOW_GLOBS for p in target.glob(pattern)})
    else:
        files = [target / name for name in DEFAULT_WORKFLOWS]
        missing = [p for p in files if not p.is_file()]
        if missing:
            for path in missing:
                print(f"::error::expected workflow not found: {path}")
            return 1

    if not files:
        print(f"::error::no workflow files under {target}")
        return 1

    problems: list[str] = []
    for path in files:
        problems.extend(audit(path))
        problems.extend(audit_unreachable_test_steps(path))
        problems.extend(audit_trigger_coverage(path))

    if problems:
        print("::error::CI test/failure gates are swallowed, unreachable, or not triggered.")
        print("::error::Re-adding a guard requires a `justified-guard:` comment;")
        print("::error::a test step after another test step needs `if: ${{ !cancelled() }}`;")
        print(f"::error::a `branches:` filter on a PR trigger needs a `{TRIGGER_MARKER}` comment.")
        for problem in problems:
            print(f"::error::{problem}")
        return 1

    scope = "every workflow" if sweep_all else ", ".join(DEFAULT_WORKFLOWS)
    print(f"OK: audited {scope} — no unjustified failure guards, no unreachable tests, no untriggered PRs.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
