#!/usr/bin/env python3
"""check_test_gates.py — fail if a CI test step is guarded against failing.

Until CON-97 landed, every test-producing step in this repo was wrapped in
`|| true` or `continue-on-error: true`, so `consortium` CI could not go red on
a failing test and no measurement in the migration program produced a signal.

This is the regression guard for that fix. It is deliberately dependency-free
and deliberately dumb: it does not try to understand YAML. It requires that
every swallow token in `.github/workflows/` carries an explicit justification
marker on the preceding line, so re-adding a guard is a loud, reviewable act
rather than a silent one.

Usage:
    python3 harness/ci/check_test_gates.py [--all] [workflow-dir]

    (default)  audit the workflows CON-97 un-swallowed: ci.yml and
               migration-scorecard.yml
    --all      audit every workflow in the directory, including
               nosetests.yml, which is still fully swallowed upstream

Exit codes:
    0  every swallow token is justified (or there are none)
    1  at least one unjustified swallow token
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

COMMENT_LINE = re.compile(r"^\s*#")

# How many non-comment lines may sit between a guard and its justification.
# 1 covers `continue-on-error:` sitting under its `- name:` header; 1 more covers
# a guard written as the last line of a `run:` block. Anything further away is
# treated as unjustified on purpose: the reason must be next to the guard.
MAX_JUSTIFICATION_DISTANCE = 3

WORKFLOW_GLOBS = ("*.yml", "*.yaml")

# Audited by default. `nosetests.yml` is the upstream ClusterShell mirror and is
# still swallowed end to end; it is tracked by a separate follow-up rather than
# being quietly folded into the CON-97 change.
DEFAULT_WORKFLOWS = ("ci.yml", "migration-scorecard.yml")


def is_justified(lines: list[str], index: int) -> bool:
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
        return any(JUSTIFICATION_MARKER in entry for entry in block)
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
        try:
            rel = path.relative_to(REPO_ROOT)
        except ValueError:
            rel = path
        problems.append(f"{rel}:{number}: unguarded failure swallow -> {line.strip()}")
    return problems


def main(argv: list[str]) -> int:
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

    if problems:
        print("::error::CI test/failure gates are swallowed. Re-adding a guard")
        print("::error::requires a `justified-guard:` comment explaining why.")
        for problem in problems:
            print(f"::error::{problem}")
        return 1

    scope = "every workflow" if sweep_all else ", ".join(DEFAULT_WORKFLOWS)
    print(f"OK: audited {scope} — no unjustified failure guards.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
