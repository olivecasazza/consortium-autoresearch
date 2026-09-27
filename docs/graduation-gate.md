# CON-4 graduation gate

**Status: the gate is wired. It reports `FAIL`. The side-by-side oracle stays.**

This note states the retirement floor for the ClusterShell side-by-side
harness, who approved that floor, and what deleting the harness would destroy.

- **Issue:** [CON-199](/CON/issues/CON-199) (split out of the CON-4 `plan` document as "CON-4b")
- **Parent:** [CON-4](/CON/issues/CON-4) — Clustershell tests migration
- **Command:** `python harness/graduation_gate.py`
- **Config:** [`harness/graduation-gate.toml`](../harness/graduation-gate.toml)
- **Reviewed decisions:** [`harness/graduation-exemptions.toml`](../harness/graduation-exemptions.toml)
- **Tests:** [`harness/test_graduation_gate.py`](../harness/test_graduation_gate.py)

## Who approved the floor

| | |
| --- | --- |
| **Floor** | ≥ 80% of upstream test methods mapped **and** passing |
| **Approved by** | CTO, in the CON-4 `plan` document, revision 1 |
| **Approved on** | 2026-07-18 |
| **Implemented by** | [CON-199](/CON/issues/CON-199) |

The floor is the one proposed in the CON-4 assessment ("Proposed floor: ≥ 80% of
upstream methods mapped AND passing in both the Rust-port run and the
`CONSORTIUM_BACKEND=rust` Python run, with no regressions vs
`CONSORTIUM_BACKEND=python`"). It was carried over unchanged. Changing it is a
board decision, not a code change: edit `min_method_coverage` in
`harness/graduation-gate.toml`, and expect the same edit here.

Retiring the harness deletes a correctness oracle, so the *retirement* also needs
board sign-off (a `request_confirmation` on CON-4) even once this gate passes.
A green gate is a precondition for that conversation, not a substitute for it.

## Current measurement

Measured 2026-09-27 on `consortium-autoresearch` at `4e819e7` (the CON-198
branch), upstream `cea-hpc/clustershell` @ `v1.9.3`, on a tree where
`cargo test -p consortium-crate` builds and both Python legs were executed
locally. **The gate reports `FAIL` on every criterion except the two that are
trivially satisfied.**

| Metric | CON-4 plan (2026-07-18) | Re-measured (2026-09-27) |
| --- | --- | --- |
| Upstream test classes | 38 | **59** |
| Upstream test cases | 582 | **1075** |
| Classes tracked in `TEST_MAPPING.toml` | 38 | **37** |
| Mapping entries | 582 | 582 |
| Test cases with a Rust mapping | 37 | 37 |
| Mapped coverage | 6.4% | **3.4%** (37 / 1075) |
| Classes fully ported | 0 | **0** |
| Classes with zero mapping | 34 | **55** of 59 |
| Critical classes (`Task*`/`Tree*`/`CLI*`/`MsgTree*`/`NodeSet*Group*`) | — | **44**, none ported |
| **Gate coverage** (mapped **and** passing in both runs) | — | **0.0%** (0 / 1075) |
| Gate regressions | — | **11** |
| Reviewed decisions on file | — | **0** |

Test-run legs, same tree:

| Leg | tests | failures | errors |
| --- | --- | --- | --- |
| `cargo test -p consortium-crate` (Rust-port) | 405 | 8 | 0 |
| `CONSORTIUM_BACKEND=python` (baseline) | 1075 | 289 | 1 |
| `CONSORTIUM_BACKEND=rust` | 44 | 11 | 33 |

Reading these honestly:

- **The Rust-port leg is healthy** — 405 tests, 8 failures, all doctests. This
  is CON-198's fix working: before it, the same command produced
  `tests="0"`.
- **The baseline leg is red**, and mostly it is the oracle talking: 289 failures
  out of 1075 on the *pure-Python* backend. This is CON-115's territory.
- **The Rust-backend leg is not a measurement at all** — 33 collection errors
  means pytest never imported most of the suite. This is CON-116. The gate's
  criterion 3 correctly refuses to read it, which is why coverage reads 0.0%
  rather than something flattering.
- Coverage is 0.0%, not 3.4%, because a mapping is not coverage: the 37 mapped
  cases also have to *pass* in both legs, and the Rust-backend leg has not run
  them.

The headline is therefore worse than the plan assumed, not better: the oracle is
**45.9% larger than the mapping has ever seen**, and mapped coverage is 3.4%
rather than 6.4%.

## What the gate measures

One command, one verdict, exit 0 on `PASS` and 1 on `FAIL`:

```console
$ python harness/graduation_gate.py
GRADUATION GATE: FAIL  coverage=0.0% (required 80%)  covered=0/1075  regressions=0  critical_classes_gap=0/44
```

The first line is the whole contract: a single `PASS`/`FAIL` token plus the
measured-vs-required numbers. The rest of the output is the per-criterion
breakdown for a human.

It is **read-only and idempotent**. It reads the tree and the JUnit XML in
`results/`, and writes nothing except `--json-out` when you ask for it. It never
runs tests, never mutates the checkout, and is safe to run on a PR. CI runs it
after the scorecard legs have produced their JUnit files.

### Criterion 1 — coverage

`min_method_coverage = 0.80` (80%). A method counts only if it is **all three**
of these, which is why the number is far below the 3.4% mapping rate:

1. mapped to at least one Rust test in `TEST_MAPPING.toml`;
2. **passing** in the Rust-port run (`results/rust-unit.xml`);
3. **passing** in the `CONSORTIUM_BACKEND=rust` Python run
   (`results/python-rust.xml`).

Two consequences worth stating plainly:

- The denominator is the upstream test tree, re-derived from the AST of
  `tests/` on every run — not `TEST_MAPPING.toml`. Deleting a mapping entry
  cannot shrink the denominator, so the coverage ratio cannot be improved by
  editing the mapping.
- A mapping entry pointing at a Rust test that does not exist, or that fails, is
  **not** coverage. `range_set::tests::test_copy` counts only if that test ran
  and passed.

### The denominator is 1075, not 582

`TEST_MAPPING.toml` has 582 entries across 37 classes, and that number has been
quoted as "the upstream test surface" since the CON-4 assessment. It is not.
The oracle actually runs **1075 test cases across 59 classes**, and the gap is
two defects in `harness/generate_test_mapping.py`'s inventory:

1. **The class filter is `class.name.endswith("Test")`.** Upstream does not
   follow that convention. `CLIClushTest_A`, `CLIClubakTestGroupsConf`,
   `CLINodesetGroupResolverTest1`, `TaskLocalEnginePollTest` and 11 others are
   real test cases pytest collects. **220 of the 1075 test cases live in classes
   the filter drops** — and those are exactly the classes criterion 2 cares
   about: `CLIClushTest_A` alone is 44 cases, the `CLINodesetGroupResolverTest*`
   set is 22, `CLIClubakTestGroupsConf` is 11.

   The filter's likely origin is the root `pyproject.toml`:
   `python_classes = ["*Test"]`. But that setting does **not** apply to
   `unittest.TestCase` subclasses — pytest collects those regardless of their
   name. All 59 collected classes reach `unittest.TestCase`; 12 of them do not
   end in `Test`. So the mapping applies a rule to a category of class that the
   rule never governed, and drops real tests because of it.
2. **Only methods written directly in the class body are counted.** Most test
   methods are inherited from `TaskLocalMixin`, `TaskDistantMixin` and
   `TaskDistantPdshMixin`. Resolving the inheritance adds **490 more cases**.
   (The mixins themselves subclass `object`, not `TestCase`, so pytest does not
   collect them either — the concrete subclasses are what run.)

Together that is 493 of 1075 test cases — **45.9% of the oracle's surface — that
the mapping has never tracked.** The gate resolves the inheritance and does not
apply the name filter, so its inventory matches pytest's collection exactly
(1075 = 1075, asserted by `test_the_inventory_counts_inherited_and_non_Test_named_classes`).

The fix to `generate_test_mapping.py` itself is *not* in this issue — it changes
the scorecard's mapping artifact, which belongs to the scorecard-step owners. The
gate is deliberately independent of it: it re-derives the truth from the tree, so
the floor cannot be met by inheriting the mapping's blind spots.

Mapped coverage is therefore **3.4%** (37 of 1075), not 6.4%.

### Criterion 1b — no regressions

Any method that passes under `CONSORTIUM_BACKEND=python` but fails or errors
under `CONSORTIUM_BACKEND=rust` fails the gate. One regression is one
regression; there is deliberately no tolerance knob. Methods that fail on
*both* backends are upstream/oracle problems, not Rust regressions, and do not
count here.

### Criterion 2 — no unmapped critical paths

Every upstream class matching `critical_class_globs` — `Task*`, `Tree*`, `CLI*`,
`MsgTree*`, `NodeSet*Group*` — must be fully covered, **or** carry an entry in
`harness/graduation-exemptions.toml`.

An exemption must be complete: `class`, `decision` (`accept` or `defer`),
`reason`, `reviewer`, `date`, `issue`. The gate rejects an entry missing any of
those, rejects an unknown `decision`, and rejects an entry for a class that
matches none of the critical globs. "We didn't get to it" is not a decision; a
reviewed, recorded judgement that the Rust feature set covers the behavior by
other means is. The file is currently **empty**, which is the honest state: 44
critical classes, none ported.

### Criterion 3 — the signal is trustworthy

This is the criterion that earns the rest of them the right to be believed.

CON-198 ([CON-4a](/CON/issues/CON-198)) found that `run_comparison.py` ran
`cargo test -p consortium`, which cargo resolves to `consortium-py` rather than
the core package `consortium-crate`. The Rust-unit leg measured nothing and
wrote `<testsuite name="consortium" tests="0" .../>` — a *passing* step that
proved nothing. Before the fix the file had zero tests; after, 405.

So the gate refuses to read an absent or partial signal:

- an empty or missing `rust-unit.xml` is a `FAIL`, not a silent `PASS`;
- a collection error in either Python leg is a `FAIL`, because a run that never
  imported half the suite cannot measure coverage.

This means the gate fails loudly while CON-115 and CON-116 are open, which is
the correct behaviour: an untrustworthy measurement must not be able to produce
a green light.

## When the gate reports `PASS`

`PASS` means: the Rust feature set is verified against the upstream ClusterShell
tests well enough that keeping the side-by-side oracle is no longer buying
anything. It does **not** by itself authorize deleting the harness.

## What retiring the harness would delete

This is the thing being weighed, per the acceptance criteria. From the current
tree, retiring the side-by-side harness means deleting or disabling:

| What | Size | What it is |
| --- | --- | --- |
| `harness/run_comparison.py` | 154 lines | the dual-backend runner |
| `harness/render_summary.py` | 224 lines | the side-by-side scorecard renderer |
| `harness/cargo_to_junit.py` | 58 lines | the Rust-leg JUnit adapter |
| the `CONSORTIUM_BACKEND=python` run in `.github/workflows/migration-scorecard.yml` | 1 step | the baseline leg |
| the `CONSORTIUM_BACKEND=rust` run in the same workflow | 1 step | the Rust leg |
| `UPSTREAM_REF` as a live pin | 1 file | becomes historical rather than a fetch target |

**The oracle itself does not go away with the harness, and that is the point.**
`lib/ClusterShell/` (12 modules, 1.4 MB of pure-Python ClusterShell) and the
35 synced `tests/*.py` files — 59 test classes, 1075 test cases — are the thing
the Rust port is checked *against*. The harness is only the machinery that runs
both sides and compares them.

So the real question CON-4 has to answer is not "can we delete 436 lines of
Python?" It is: **once the side-by-side comparison stops running on every PR,
what is left that would catch a divergence between the Rust implementation and
upstream ClusterShell's behavior?** Today the answer is "the mapping in
`TEST_MAPPING.toml` plus the 3.4% of the oracle it covers" — which is why the
floor is 80% and not lower. At 3.4%, retiring the harness would leave the
divergence detector switched off while 96.6% of what it detects is unported and
45.9% of the oracle was never even in its field of view.

Note also that `TEST_MAPPING.toml` is regenerated on every scorecard run
(`harness/generate_test_mapping.py --update`), so the mapping is a live artifact
rather than a curated one. The gate reads the regenerated file, which is why the
CI step order matters: generate the mapping, then run the gate.

## CI wiring

In `.github/workflows/migration-scorecard.yml`, after the scorecard legs:

1. `Test the graduation gate` — runs the gate's own self-tests. These are what
   stop a `FAIL` from being "fixed" by weakening the gate: they assert the gate
   fails on an empty Rust leg, on a collection error, on a single regression, on
   an unreviewed exemption, and on a mapping to a nonexistent Rust test — and
   that it still reports `PASS` on a fully-ported world and exactly on the
   floor.
2. `CON-4 graduation gate` — runs the gate, writes `results/graduation-gate.json`,
   and publishes `graduation_gate=PASS|FAIL` as a job output plus a step-summary
   table.
3. The JSON report is uploaded as the `graduation-gate-report` artifact,
   retained 90 days, so a `PASS` can be traced to the run that produced it.

**The gate step is `continue-on-error: true` on purpose.** At 3.4% coverage it is
*supposed* to report `FAIL`; a hard-failing required check would paint every PR
red until the porting stream reaches 80%, and a permanently-red check is one
nobody reads. The signal is wired, reported, archived, and consumed as a job
output — which is what CI needs in order to gate a decision.

**To make it blocking:** delete the one `continue-on-error: true` line on the
`CON-4 graduation gate` step, and add `Graduation gate` to the branch
protection required checks. Do that when the gate first reports `PASS`, at which
point it is enforcing a floor the code has actually met.

## Reproducing the measurement

```console
$ python harness/generate_test_mapping.py          # refresh the mapping
$ cargo test -p consortium-crate \
    | python harness/cargo_to_junit.py > results/rust-unit.xml
$ CONSORTIUM_BACKEND=python PYTHONPATH=lib pytest tests/ \
    --junit-xml=results/python-original.xml
$ CONSORTIUM_BACKEND=rust pytest tests/ \
    --junit-xml=results/python-rust.xml
$ python harness/graduation_gate.py
```

CI runs exactly this sequence in `migration-scorecard.yml`.
