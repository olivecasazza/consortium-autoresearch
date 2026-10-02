#!/usr/bin/env python3
"""Tests for check_test_gates.py.

The harness is the regression guard for CON-97 (swallowed test gates) and
CON-209 (stacked PRs running no CI at all). A guard that cannot itself fail is
the same defect it exists to catch, so these tests pin each audit to a fixture
that must go red.

Dependency-free (stdlib `unittest` only) so it runs in the same job as the
harness itself, with nothing to install.

Usage:
    python3 -m unittest discover -s harness/ci -p 'test_*.py'
    python3 harness/ci/test_check_test_gates.py
"""

from __future__ import annotations

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_test_gates as gates  # noqa: E402


def write(directory: Path, name: str, body: str) -> Path:
    path = directory / name
    path.write_text(body, encoding="utf-8")
    return path


class PrTriggerAudit(unittest.TestCase):
    """CON-209: a `branches:` filter on `pull_request:` is a silent no-CI gate."""

    def test_flags_pull_request_restricted_to_default_branches(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "on:\n"
                "  push:\n"
                "    branches: [main, master]\n"
                "  pull_request:\n"
                "    branches: [main, master]\n",
            )
            problems = gates.audit_pr_trigger(path)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("pull_request trigger is restricted", problems[0])
        self.assertIn("ci.yml:5", problems[0])

    def test_flags_filter_beneath_siblings(self) -> None:
        """A `paths-ignore:` before `branches:` must not hide the filter."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "scorecard.yml",
                "on:\n"
                "  pull_request:\n"
                "    paths-ignore:\n"
                "      - 'doc/**'\n"
                "    branches: [main, master]\n",
            )
            problems = gates.audit_pr_trigger(path)
        self.assertEqual(len(problems), 1, problems)

    def test_passes_unfiltered_pull_request(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "on:\n"
                "  pull_request:\n"
                "  workflow_dispatch:\n",
            )
            self.assertEqual(gates.audit_pr_trigger(path), [])

    def test_passes_unfiltered_pull_request_with_paths_ignore(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "scorecard.yml",
                "on:\n"
                "  pull_request:\n"
                "    paths-ignore:\n"
                "      - 'doc/**'\n"
                "  push:\n"
                "    branches: [main, master]\n",
            )
            self.assertEqual(gates.audit_pr_trigger(path), [])

    def test_does_not_flag_push_branch_filter(self) -> None:
        """`push:` legitimately filters to default branches; only PR must not."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "on:\n  push:\n    branches: [main, master]\n",
            )
            self.assertEqual(gates.audit_pr_trigger(path), [])


class SwallowAudit(unittest.TestCase):
    """CON-97: an unjustified swallow token must still go red."""

    def test_flags_bare_or_true(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n  a:\n    steps:\n      - run: pytest tests/ || true\n",
            )
            problems = gates.audit(path)
        self.assertEqual(len(problems), 1, problems)

    def test_flags_bare_continue_on_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n  a:\n    steps:\n"
                "      - name: Run tests\n        continue-on-error: true\n",
            )
            problems = gates.audit(path)
        self.assertEqual(len(problems), 1, problems)

    def test_accepts_justified_guard(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n  a:\n    steps:\n"
                "      - name: Notify Slack\n"
                "        # justified-guard: Slack is a courtesy, not a gate.\n"
                "        continue-on-error: true\n",
            )
            self.assertEqual(gates.audit(path), [])

    def test_ignores_tokens_inside_comments(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "# this workflow used to end in `|| true`\njobs:\n  a:\n    steps: []\n",
            )
            self.assertEqual(gates.audit(path), [])


class UnreachableTestStepAudit(unittest.TestCase):
    """CON-97: a test step after a sibling test step is skipped, not red."""

    def test_flags_second_test_step_without_condition(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  unit:\n"
                "    steps:\n"
                "      - name: Run unit tests\n"
                "        run: cargo test\n"
                "      - name: Run integration tests\n"
                "        run: cargo test --test it\n",
            )
            problems = gates.audit_unreachable_test_steps(path)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("can only be skipped", problems[0])

    def test_accepts_uncancelled_condition(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  unit:\n"
                "    steps:\n"
                "      - name: Run unit tests\n"
                "        run: cargo test\n"
                "      - name: Run integration tests\n"
                "        if: ${{ !cancelled() }}\n"
                "        run: cargo test --test it\n",
            )
            self.assertEqual(gates.audit_unreachable_test_steps(path), [])


class HashFilesSelfHostedAudit(unittest.TestCase):
    """CON-415: `hashFiles()` aborts a whole self-hosted job at template time."""

    def test_flags_hash_files_in_self_hosted_job(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  tool-integration:\n"
                "    runs-on: nix-builder\n"
                "    steps:\n"
                "      - name: Cache cargo\n"
                "        uses: actions/cache@v4\n"
                "        with:\n"
                "          key: ${{ runner.os }}-${{ hashFiles('**/Cargo.lock') }}\n",
            )
            problems = gates.audit_hash_files_on_self_hosted(path)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("hashFiles() in self-hosted job 'tool-integration'", problems[0])
        self.assertIn("ci.yml:8", problems[0])

    def test_allows_hash_files_on_github_hosted_job(self) -> None:
        """GitHub-hosted runners ship node20, so the same call is fine there."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  unit:\n"
                "    runs-on: ubuntu-latest\n"
                "    steps:\n"
                "      - name: Cache cargo\n"
                "        uses: actions/cache@v4\n"
                "        with:\n"
                "          key: ${{ runner.os }}-${{ hashFiles('**/Cargo.lock') }}\n",
            )
            self.assertEqual(gates.audit_hash_files_on_self_hosted(path), [])

    def test_flags_self_hosted_label(self) -> None:
        """`self-hosted` is the label the pool is addressed by directly."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  a:\n"
                "    runs-on: self-hosted\n"
                "    steps:\n"
                "      - run: echo ${{ hashFiles('a') }}\n",
            )
            problems = gates.audit_hash_files_on_self_hosted(path)
        self.assertEqual(len(problems), 1, problems)

    def test_does_not_carry_self_hosted_status_across_jobs(self) -> None:
        """A GitHub-hosted job after a self-hosted one must not inherit it."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  a:\n"
                "    runs-on: nix-builder\n"
                "    steps:\n"
                "      - run: echo hi\n"
                "  b:\n"
                "    runs-on: ubuntu-latest\n"
                "    steps:\n"
                "      - run: echo ${{ hashFiles('a') }}\n",
            )
            self.assertEqual(gates.audit_hash_files_on_self_hosted(path), [])

    def test_ignores_hash_files_mentioned_in_a_comment(self) -> None:
        """The explanation of this trap must not itself trip the audit."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  a:\n"
                "    runs-on: nix-builder\n"
                "    steps:\n"
                "      # CON-415: hashFiles() is expanded via the runner's node20.\n"
                "      - run: echo hi\n",
            )
            self.assertEqual(gates.audit_hash_files_on_self_hosted(path), [])

    def test_replaced_cache_key_is_clean(self) -> None:
        """The shipped fix: hash in a `run:` step, pass via $GITHUB_OUTPUT."""
        with tempfile.TemporaryDirectory() as tmp:
            path = write(
                Path(tmp),
                "ci.yml",
                "jobs:\n"
                "  tool-integration:\n"
                "    runs-on: nix-builder\n"
                "    steps:\n"
                "      - name: Compute cargo cache key\n"
                "        id: cargo-cache-key\n"
                "        run: echo \"key=x-$(sha256sum Cargo.lock)\" >> \"$GITHUB_OUTPUT\"\n"
                "      - name: Cache cargo\n"
                "        uses: actions/cache@v4\n"
                "        with:\n"
                "          key: ${{ steps.cargo-cache-key.outputs.key }}\n",
            )
            self.assertEqual(gates.audit_hash_files_on_self_hosted(path), [])


class RetiredWorkflowAudit(unittest.TestCase):
    """ADR 0002: a retired workflow must not come back quietly."""

    def test_clean_when_retired_workflow_is_absent(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(gates.audit_retired_workflows(Path(tmp)), [])

    def test_flags_reintroduced_retired_workflow(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            write(target, "nosetests.yml", "name: back\n")
            problems = gates.audit_retired_workflows(target)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("ADR 0002", problems[0])


class RepoWorkflows(unittest.TestCase):
    """The audit must be green against the workflows as they are committed."""

    def test_real_workflows_are_clean(self) -> None:
        workflow_dir = gates.WORKFLOW_DIR
        self.assertTrue(workflow_dir.is_dir(), workflow_dir)
        problems: list[str] = []
        # The trigger audit and the retired-workflow guard cover every workflow.
        problems.extend(gates.audit_retired_workflows(workflow_dir))
        for pattern in gates.WORKFLOW_GLOBS:
            for path in sorted(workflow_dir.glob(pattern)):
                problems.extend(gates.audit_pr_trigger(path))
                problems.extend(gates.audit_hash_files_on_self_hosted(path))
        # Swallow tokens are gated on the two real gates; every other workflow
        # is covered by the `--all` sweep, which CI does not run by design.
        for name in gates.DEFAULT_WORKFLOWS:
            path = workflow_dir / name
            problems.extend(gates.audit(path))
            problems.extend(gates.audit_unreachable_test_steps(path))
        self.assertEqual(problems, [], "\n".join(problems))

    def test_full_sweep_is_clean(self) -> None:
        """`--all` must be green, not just the default two-workflow audit."""
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            (target / "ci.yml").write_text((gates.WORKFLOW_DIR / "ci.yml").read_text())
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gates.main(["prog", "--all", str(target)]), 0)

    def test_default_audit_gates_on_a_red_workflow(self) -> None:
        """The guard itself must be able to go red (CON-97's own standard)."""
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            (target / "ci.yml").write_text(
                "jobs:\n  a:\n    steps:\n      - run: pytest tests/ || true\n"
            )
            (target / "migration-scorecard.yml").write_text("jobs:\n  a:\n    steps: []\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gates.main(["prog", str(target)]), 1)

    def test_default_audit_gates_on_a_hash_files_workflow(self) -> None:
        """`main()` must surface the CON-415 finding, not just the helper."""
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)
            (target / "ci.yml").write_text(
                "jobs:\n"
                "  a:\n"
                "    runs-on: nix-builder\n"
                "    steps:\n"
                "      - run: echo ${{ hashFiles('Cargo.lock') }}\n"
            )
            (target / "migration-scorecard.yml").write_text("jobs:\n  a:\n    steps: []\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gates.main(["prog", str(target)]), 1)


if __name__ == "__main__":
    unittest.main()
