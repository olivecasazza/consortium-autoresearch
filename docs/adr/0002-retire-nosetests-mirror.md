# ADR 0002 — Retire the `nosetests.yml` upstream mirror; run CI on every PR base

**Status:** Accepted
**Date:** 2026-09-28
**Decision owner:** DevOps Engineer ([CON-118](/CON/issues/CON-118), [CON-209](/CON/issues/CON-209))
**Implements:** [CON-118](/CON/issues/CON-118) · [CON-209](/CON/issues/CON-209)
**Depends on:** [CON-97](/CON/issues/CON-97) (un-swallowed the two real gates) · [ADR 0001](/CON/issues/CON-15) D3 (every lane unenforceable until G0 lands)

---

## 1. Context

[CON-97](/CON/issues/CON-97) un-swallowed `ci.yml` and `migration-scorecard.yml`. Two defects
survived it, and both lived in the same file: `.github/workflows/nosetests.yml`, the upstream
ClusterShell mirror.

**It could not fail on a test.** Every test-producing step was guarded, and the guard discarded
the only signal the run produced:

```
.github/workflows/nosetests.yml:100: nosetests -v --all-modules --with-coverage ... || true
.github/workflows/nosetests.yml:67:  continue-on-error: true
.github/workflows/nosetests.yml:103: continue-on-error: true
```

Because of the `|| true`, the `nosetests` summary was thrown away, so nothing in that workflow's
log could be trusted as a pass/fail statement. Its runs were red, but for setup reasons and
without any test signal.

**It was blind for stacked PRs.** Its `pull_request:` trigger was restricted to
`branches: [main, master, develop]`, so a PR targeting a feature branch ran no CI at all. This
repo stacks PRs, so every stacked PR was silently unverified against the trunk it was stacked on.
`gh pr checks` prints `no checks reported on the '<branch>' branch`, which reads as "nothing to
see" rather than "no signal" — the same shape of blindness [CON-97](/CON/issues/CON-97) and
[CON-209](/CON/issues/CON-209) are about: the absence of a result presented as the absence of a
problem.

## 2. Why not un-swallow it

[CON-118](/CON/issues/CON-118) offered two options: retire it, or un-swallow it the way
[CON-97](/CON/issues/CON-97) did. Un-swallowing was rejected on three measured grounds.

**It tests no first-party code.** `nosetests.yml` never runs `harness/sync_upstream_tests.sh`,
so it exercises whatever mirror happens to be checked in rather than the pinned upstream ref
under test. It never builds the consortium PyO3 bindings, so it cannot exercise this repo's code
at all — it exercises upstream ClusterShell's own Python. Its only real value-add was a
Python-version matrix (3.7–3.13) over a library this repo does not own.

**Its suite is already gated, harder.** `migration-scorecard.yml` syncs upstream tests at a
pinned ref and runs the same `tests/` tree under `pytest` against **both** backends, with
`fail_on_failure: true`, `fail_on_parse_error: true` and `require_tests: true`. That is a real
gate; `nosetests.yml` was a weaker duplicate of it.

**Un-swallowing would have turned `master` permanently red.** The workflow's runs were already
red for setup reasons. Removing the guard without fixing those would have traded a silent gate
for a loud one that never goes green — a regression, not a fix.

## 3. Decision

| # | Question | Decision |
|---|---|---|
| **D1** | Fate of `nosetests.yml` | **Retired.** Deleted. `migration-scorecard.yml` is the single Python gate. §4 |
| **D2** | `pull_request:` `branches:` filters | **Removed** from `ci.yml` and `migration-scorecard.yml`. A PR runs CI against whatever same-repo branch it targets. §5 |
| **D3** | How the decision is kept | **Both audits are now hard errors in `harness/ci/check_test_gates.py`**, with tests that pin each one to a fixture that must go red. §6 |

### D2 — why removing the filter adds no duplicate runs

`pull_request` fires once per PR, so a `master`-targeting PR is still built exactly once. The
`push:` trigger stays restricted to the default branches, which is what prevents the duplicate.
Widening `pull_request` therefore adds coverage without adding runs.

## 4. What replaces the retired workflow

The Python matrix is not lost; it moves to the gate that can actually fail:

- `migration-scorecard.yml` already runs the synced upstream suite under `pytest` on the
  `ubuntu-latest` Python, against both the original and the Rust backend, with a hard JUnit gate.
- If the Python-version matrix is wanted back, it belongs there — as a matrix over the
  **scorecard's** Python step, with the same `fail_on_failure` / `require_tests` gate, not as a
  separate workflow that can only ever test upstream's own library.

## 5. Trigger surface after this change

| Workflow | `pull_request:` |
|---|---|
| `ci.yml` | unfiltered |
| `migration-scorecard.yml` | unfiltered (`paths-ignore` retained) |
| `docs.yml`, `release.yml` | no `pull_request` trigger (unchanged) |

## 6. Enforcement

`harness/ci/check_test_gates.py` now carries three audits, each with a test that must go red:

| Audit | Catches |
|---|---|
| `audit()` | an unjustified `\|\| true` / `continue-on-error: true` (CON-97) |
| `audit_unreachable_test_steps()` | a test step that can only be skipped, never red (CON-97) |
| `audit_pr_trigger()` | a `branches:` filter on `pull_request:` (CON-209) |
| `audit_retired_workflows()` | a retired workflow being reintroduced (this ADR) |

`harness/ci/test_check_test_gates.py` (16 tests, stdlib `unittest`, no dependencies) pins each
audit to a fixture, and asserts the audit is green against the workflows as committed. The
guard that cannot itself fail is the same defect it exists to catch, so the tests are the
deliverable as much as the audits.

## 7. Deliberately not done

- **No `justified-guard:` marker on the retired file.** The marker exists to make re-adding a
  guard a loud, reviewable act. Deleting the workflow outright is the stronger statement, and
  `audit_retired_workflows()` makes reintroducing it a hard error rather than a finding to be
  triaged.
- **No widening of `push:`.** Restricting `push` to the default branches is what keeps a
  `master`-targeting PR from being built twice.
- **No change to `docs.yml` / `release.yml`.** Neither has a `pull_request` trigger; neither is
  a test gate.
