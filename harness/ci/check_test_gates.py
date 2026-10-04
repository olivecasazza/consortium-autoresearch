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

The third form is CON-415's: a self-hosted job that calls `hashFiles()`. The
runner expands that call through the node runtime in its own install root,
before any step runs, and current nixpkgs `github-runner` ships node24 only. One
call therefore aborts the entire job — silently, since the steps that did run
report nothing while the job sits red. It reads as runner corruption rather than
as a workflow defect, and it held a lane out of service for two days.

Usage:
    python3 harness/ci/check_test_gates.py [--all] [workflow-dir]

    (default)  audit the workflows CON-97 un-swallowed: ci.yml and
               migration-scorecard.yml
    --all      audit every workflow in the directory

Two workflows are *retired* by decision (ADR 0002) and must not be
reintroduced: `nosetests.yml`, the upstream ClusterShell mirror, and any
attempt to bring it back fails the audit. The `pull_request:` trigger of every
workflow is also audited: a `branches:` filter there means a PR targeting a
feature branch runs no CI and reports nothing, which reads as "nothing to see"
rather than "no signal". CON-209 removed those filters; this audit is what keeps
them from coming back.

Exit codes:
    0  every swallow token is justified (or there are none) and every
       non-first test step in a job is reachable after a sibling failure
    1  at least one unjustified swallow token, or at least one test step that
       can only ever be skipped
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
JOB_RUNS_ON = re.compile(r"^ {4}runs-on:\s*(.*)$")

# CON-415. `hashFiles()` is not a workflow-engine function: the runner evaluates
# it during template expansion by spawning `externals/node20/bin/node` from its
# own install root, before any step executes. A self-hosted runner built from
# current nixpkgs ships node24 only (node20 left the store at EOL), so a single
# `hashFiles()` anywhere in a job aborts the whole job with
#
#   ##[error]The template is not valid. ... An error occurred trying to start
#   process '.../lib/externals/node20/bin/node' ... No such file or directory
#
# Steps before the `hashFiles()` step still run, which is what makes this read
# as "the runner is broken" rather than "one line in one job is wrong". It is
# also a *silent* gate: the job is red, but no step ever reported a test, so
# the lane carries no signal while looking like an infrastructure outage.
#
# GitHub-hosted runners ship node20, so the same call is fine there. Only jobs
# pinned to a self-hosted label are audited, which is why this needs a
# `runs-on:` check rather than a blanket ban.
HASH_FILES = re.compile(r"\$\{\{\s*hashFiles\s*\(", re.IGNORECASE)

# Runners whose JS-action runtime comes from the runner's own install root,
# and so are subject to the node20 removal above.
SELF_HOSTED_POOL = re.compile(
    r"self-hosted|nix-builder|nixos|nix-remote-builder", re.IGNORECASE
)

# Audited by default: the two workflows that are real gates.
DEFAULT_WORKFLOWS = ("ci.yml", "migration-scorecard.yml")

# Retired by decision, not by accident (ADR 0002). `nosetests.yml` was the
# upstream ClusterShell mirror: it never synced upstream tests, never built the
# consortium bindings, ran the suite under `|| true`, and was the last workflow
# still restricted to default-branch `pull_request:` triggers. Retiring it
# removes the last file that neither audit can pass. Reintroducing it is a
# regression, so it is a hard error rather than a finding to be triaged.
RETIRED_WORKFLOWS = {
    "nosetests.yml": (
        "retired by ADR 0002 - it is the upstream ClusterShell mirror, its suite "
        "is already gated by migration-scorecard.yml (synced at a pinned ref, run "
        "by pytest against both backends with fail_on_failure/require_tests), and "
        "it can only ever exercise upstream ClusterShell's own Python. Restore the "
        "Python matrix in migration-scorecard.yml instead."
    ),
}


def audit_retired_workflows(target: Path) -> list[str]:
    """Fail if a workflow retired by ADR is back in the directory."""
    return [
        f"{rel(target / name)}: {reason}"
        for name, reason in sorted(RETIRED_WORKFLOWS.items())
        if (target / name).exists()
    ]


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


def audit_hash_files_on_self_hosted(path: Path) -> list[str]:
    """Flag `hashFiles()` in a job pinned to a self-hosted runner.

    A `hashFiles()` call is expanded by the runner itself, through the node
    runtime in the runner's install root, before any step runs. Self-hosted
    runners built from current nixpkgs do not ship that runtime, so the call
    takes the whole job down with a "template is not valid" error that reads
    like runner corruption rather than a workflow defect.

    Same dumb indentation parsing as the rest of this file: track the current
    job's `runs-on:` and report every `hashFiles()` line inside a self-hosted
    job. A comment is skipped, so the audit can be discussed in the file.
    """
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    job: str | None = None
    self_hosted = False
    for number, line in enumerate(lines, start=1):
        if COMMENT_LINE.match(line):
            continue
        header = JOB_HEADER.match(line)
        if header:
            job, self_hosted = header.group(1), False
            continue
        runs_on = JOB_RUNS_ON.match(line)
        if runs_on and job is not None:
            self_hosted = bool(SELF_HOSTED_POOL.search(runs_on.group(1)))
            continue
        if not self_hosted or not HASH_FILES.search(line):
            continue
        problems.append(
            f"{rel(path)}:{number}: hashFiles() in self-hosted job '{job}' -> "
            f"{line.strip()} — the runner expands this through its own "
            f"node20 runtime, which current nixpkgs `github-runner` no longer "
            f"ships, so it aborts the whole job before any step runs. Compute "
            f"the hash in a `run:` step (e.g. `sha256sum`) and pass it through "
            f"$GITHUB_OUTPUT instead."
        )
    return problems


PR_TRIGGER = re.compile(r"^ {2}pull_request:\s*$")
PR_BRANCHES = re.compile(r"^ {4}branches:")


def audit_pr_trigger(path: Path) -> list[str]:
    """Flag a `pull_request:` trigger that is restricted to default branches.

    Deliberately the same dumb indentation parsing as the rest of this file: it
    only looks for a `pull_request:` key at the `on:` level followed by a
    `branches:` key one level deeper, and never evaluates YAML.
    """
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    for number, line in enumerate(lines):
        if not PR_TRIGGER.match(line):
            continue
        # A `branches:` filter may sit directly under `pull_request:` or under a
        # `paths-ignore:`-style sibling; scan the block that follows it.
        probe = number + 1
        while probe < len(lines) and (lines[probe].startswith("    ") or not lines[probe].strip()):
            if PR_BRANCHES.match(lines[probe]):
                problems.append(
                    f"{rel(path)}:{probe + 1}: pull_request trigger is restricted to "
                    f"branches -> PRs targeting a feature branch run no CI and report "
                    f"nothing, which reads as 'nothing to see'. Drop the `branches:` "
                    f"filter so a PR runs CI against whatever same-repo branch it targets."
                )
                break
            probe += 1
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

    retired = audit_retired_workflows(target)
    if retired:
        print("::error::a workflow retired by ADR has been reintroduced.")
        for problem in retired:
            print(f"::error::{problem}")
        return 1

    problems: list[str] = []
    for path in files:
        problems.extend(audit(path))
        problems.extend(audit_unreachable_test_steps(path))
        problems.extend(audit_pr_trigger(path))
        problems.extend(audit_hash_files_on_self_hosted(path))

    if problems:
        print("::error::CI test/failure gates are swallowed or unreachable.")
        print("::error::Re-adding a guard requires a `justified-guard:` comment;")
        print("::error::a test step after another test step needs `if: ${{ !cancelled() }}`.")
        print("::error::a self-hosted job must not call `hashFiles()`; the runner")
        print("::error::expands it via its own node20 runtime and aborts the job.")
        for problem in problems:
            print(f"::error::{problem}")
        return 1

    scope = "every workflow" if sweep_all else ", ".join(DEFAULT_WORKFLOWS)
    print(f"OK: audited {scope} — no unjustified failure guards.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
