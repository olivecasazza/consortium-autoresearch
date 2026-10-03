# Testing Requirements & Success Criteria — Consortium Test Framework

**Status:** rubric of record for the framework decision ([CON-16](/CON/issues/CON-16)) and the
baseline lock-in for the regression gates ([CON-20](/CON/issues/CON-20), [CON-21](/CON/issues/CON-21)).
**Owner:** CTO. **Scope:** requirements only — this document scores approaches, it does not pick one.

Evidence base: `olivecasazza/consortium-autoresearch` @ `f3acd8e`, plus the
[`test-asset-inventory`](/CON/issues/CON-17#document-test-asset-inventory) (revision `fd7b4e56`),
plus CON-18's measured corrections @ `e548a03` — a branch that was never pushed and no longer exists
in any ref of this repository (see the provenance note below).
Every number in §3 is either **measured** (reproducible command given) or **derived** (formula shown).
Numbers that are neither are labelled **target** and are explicitly *not* yet evidence.

> **Provenance note (added when this document landed on `master`, CON-123).** Two commits cited
> throughout this rubric are **not reachable from GitHub**:
>
> - `e548a03` — `tests/rubric_scenarios.rs` (S2/S3 reference scenarios) and `215c65f` —
>   `src/report.rs` + `tests/report_schema.rs` (the `RunReport` schema), both on the local branch
>   `feat/con-18-report-schema`, were never pushed and are absent from every branch and PR ref in
>   this repository. The only surviving copy of `9bd3458` (this document) is on the ref of the
>   withdrawn PR #28.
> - Every §3 figure attributed to `e548a03` (the 115-test count, the `report_schema` and
>   `rubric_scenarios` breakdowns, and the S2/S3 corrections in §5) is therefore **not currently
>   reproducible from a clean checkout**, and the `cargo test` commands quoted alongside them will
>   not find those suites.
>
> The rubric's *decision content* — the framework scoring, the budget rubric, and the S1–S3
> acceptance criteria — is unaffected and is what this landing makes readable. What is still missing
> is the code those numbers measure; that gap is tracked on [CON-123](/CON/issues/CON-123) and is
> not closed by this commit. Treat `e548a03`-attributed counts as **historical measurements**, not
> as reproducible evidence, until those commits are reconstructed or re-measured.

> **Re-measured at `f3acd8e` (2026-09-27).** The measured baseline moved substantially since `d087a48`:
> the suite grew **35 → 89 tests** and **5.2 s → 111 s** of test time, and `link.rs` / `dag_sim.rs`
> are now **committed and green** rather than uncommitted. §3, §4.1, and §5 are updated to match.
> The document's *structure*, thresholds, and verdicts are unchanged. The shared working tree **still
> does not compile** (`consortium/src/task_timer.rs:91` — `dyn EventHandler: Debug`, 1 error), so
> all measurements were taken from a clean `git worktree` at `f3acd8e`, not the dirty tree.
>
> **Updated at `e548a03` from CON-18's measured corrections.** The suite is now **115 tests**
> (89 + 10 `report_schema.rs` + 10 `rubric_scenarios.rs` + 6 `report.rs` unit), and S1–S3 are
> **built and measured** rather than projected. **S2's two defective rows were removed** after they
> were shown unsatisfiable — see §5 S2. Where a CON-18 measurement differs from the `f3acd8e`
> figure, the CON-18 figure is cited as the later measurement and the discrepancy is noted rather
> than silently overwritten.

---

## 1. Why this document exists

The framework decision has to choose between extending `consortium-fanout-sim` (deterministic
simulation), `consortium-test-harness` (containerized real nodes), or a hybrid. That choice is
worthless if it is argued from impressions. This document fixes, in advance:

1. six scoring rows with falsifiable thresholds (§4),
2. four reference scenarios with numeric pass marks that the chosen framework must satisfy (§5),
3. the cost ceiling any candidate must fit inside (§3.3),
4. the measurement procedure, so two people score the same candidate the same way (§6).

Scenarios S1–S4 are the **baseline lock-in**: once adopted, their thresholds are regression
contracts. A framework that cannot meet them is not a candidate, regardless of how elegant it is.

---

## 2. Premise corrections (read before scoring)

Three claims circulating in the decision thread are false against the code. Scoring against them
would bias the matrix toward an unnecessary rewrite.

| Claim | Reality | Consequence for scoring |
|---|---|---|
| "`madsim` is already used in `consortium-fanout-sim`" | **No `madsim` dependency exists.** Deps are `consortium-nix`, `rand`, `rand_chacha`. The only two `madsim` mentions in the repo are comments in `crates/consortium-nix/src/cascade.rs:23,325`. | There is **no virtual clock** and none is claimed. Do not score a candidate for "virtual time" as if it were free — see §4.3, it is a cost line. |
| "The sim already scales to N=10k" | **No test asserts N > 1024.** All three scale-aware files pin `SCALES = [64,256,1024]`. N=10 000 was measured here for the first time, and independently re-measured by CON-18: it works but costs **~90–100 s wall and 3.3 GB RSS** in release (§3.2). | Treat N=10k as a *requirement to be sized*, not a number to copy. It is feasible, and it is far too expensive to be a blocking per-PR gate. The blocker is the O(N²) edge representation, not the cascade. |
| "Simulated time ≈ test wall-time" | The sim has **time as data**, never a wall clock: `NetworkProfile::latency_of` returns `Duration`, consumed arithmetically in `executor.rs:92-94`. | `sum(round_durations)` is *modelled fleet time*; process wall-time is a *different quantity*. Conflating them is how "the sim is fast" conclusions go wrong. §3.2 reports both. |
| "The suite is fast enough to gate per-PR" | The suite is **~111 s of test time at `f3acd8e`**, up from 5.2 s at `d087a48` — and `ci.yml` still swallows all six test failures. | 94 % of that time is in two files added since the last measurement. The suite grew 2.5× while remaining **non-gating**. Sizing the budget from the old 5.2 s number understates the PR lane by ~20×. |


**Reproducibility contract (what determinism actually means here).** Determinism comes from seeded
`rand_chacha::ChaCha8Rng` via `fixtures::rng_from_seed`, not from a virtual clock. The sim has
**time as data** and never reads a wall clock. The contract to score against is therefore:

> For a fixed `ScenarioConfig` (including `seed`), two runs produce **byte-identical** `CascadeResult`
> — same `rounds`, same `round_durations`, same converged set, same error tree.

Asserted at two levels, both now tested rather than asserted in prose:

- **Result level** — `scenario_is_deterministic_in_seed` (`fuzz.rs:259`, proptest over
  `seed in 0u64..u64::MAX`, `n in 8..=64`), tightened to full set-equality rather than `len()`.
- **Report level** — `RunReport::to_json` emits every node-keyed collection in node-id order, so two
  runs of one seed produce **byte-identical JSON**, pinned by
  `the_report_is_byte_identical_across_repeated_runs_of_one_seed` (`report_schema.rs:78`, at N ∈ {64,
  256}, asserting both `to_json` *and* `to_markdown`).

**Why the report level is the one that matters.** A byte-identical result is a reproducibility claim; a
byte-identical **report** is a regression-baseline claim. It means a stored `.json` can serve directly
as the baseline artifact CON-20 diffs against, with no canonicalisation step and no tolerance window.
This is what makes O3 (§4.5) cheap to build rather than a research project.

---

## 3. Measured baseline

### 3.1 Current test suite

`cargo test -p consortium-fanout-sim` @ `f3acd8e` — **89 passed, 0 failed, 1 ignored (90 total)**:

| Target | Tests | Wall | Δ vs `d087a48` |
|---|---:|---:|---|
| unit (`src/{lib,executor,fixtures,scenario,link}.rs`) | 51 | 0.20 s | +35 (the `link.rs` inline suite) |
| `tests/correctness.rs` | 5 | 0.19 s | — |
| `tests/contention.rs` | 5 | 0.06 s | — |
| `tests/fuzz.rs` | 3 | 1.17 s | — |
| `tests/peer_ssh.rs` | 3 | 0.00 s | — |
| `tests/scale_smoke.rs` | 2 | 4.78 s | — |
| **`tests/deploy_dag.rs`** | **12** | **72.29 s** | **new** |
| **`tests/scale_failures.rs`** | **7** | **31.87 s** | **new** |
| doctest | 1 (+1 ignored) | 0.40 s | — |
| **total** | **89** | **~111 s** | was 35 / 5.2 s |

Two facts the decision needs, and they are the reason §3.3's budget is stated the way it is:

1. **The cost centre moved.** At `d087a48`, `scale_smoke.rs` was 90 % of the suite. It is now **3.6 %**
   (4.78 s of 111 s). The two files added since — `deploy_dag.rs` and `scale_failures.rs` — are
   **94 %** of test time. Any CI budget decided from the old number is wrong by an order of magnitude.
2. **Individual tests are already near the nextest kill line.** Slowest isolated debug runs, measured
   one `--exact` at a time: `widening_parallelism_shortens_the_deploy_when_the_link_is_not_the_bottleneck`
   **12 s**, `reports_are_reproducible_at_every_scale` **12 s**,
   `loss_inflation_shows_up_in_copy_timing_at_every_scale` **11 s**,
   `several_dead_nodes_still_orphan_reroot_at_scale` **23 s**. The last of these is the single most
   expensive test in the crate and is **not** a scale test — it is a 3-scale failure sweep.

Both new files cap at `SCALES = [64, 256, 1024]`. **Nothing in the asserted suite exercises N > 1024.**

**Refreshed at `e548a03`: 89 → 115 tests.** CON-18 added 26 more: 10 in `report_schema.rs`, 10 in
`rubric_scenarios.rs` (this rubric's S2 and S3, §5), and 6 unit tests in `report.rs`. The cost
distribution below is the one that matters for §3.3, and the additions do not change it: the
N=1024 sweep remains the cost centre, and the new files are single-scale or small-N by construction —
`rubric_scenarios.rs` is N=256 throughout, which is why S2 and S3 measure 0.30 s each.

> **Why the count kept moving.** Three different counts are quoted across the decision thread — 35
> (the original briefing), 89 (this table at `f3acd8e`), 105 (CON-18's first report at `e548a03`),
> and 115 (final at `e548a03`, after the 10 `rubric_scenarios.rs` tests landed). A **static count of
> `#[test]` attributes at `e548a03` is 115** (`src/`: 58, `tests/`: 57). Quote 115 against a named
> commit, never a bare number — the suite has roughly tripled inside three days and a count without a
> SHA is meaningless.


### 3.2 Scale-ceiling sizing sweep

Reproduce: `cargo run --release --example scale_sizing -p consortium-fanout-sim`.
Uniform 1 GB/s edges, `MaxBottleneckSpanning`, seed `0xdeadbeef`, each N in its own process.

| N | rounds | ⌈log₂N⌉ | modelled fleet time | **release wall** | **debug wall** | peak RSS |
|---:|---:|---:|---:|---:|---:|---:|
| 64 | 6 | 6 | 600 s | 1.2 ms | — | 4.2 MB |
| 256 | 8 | 8 | 800 s | 82 ms | 267 ms | 6.8 MB |
| 1024 | 10 | 10 | 1 000 s | 404 ms | 3 988 ms | 55 MB |
| 2048 | 11 | 11 | 1 100 s | 5 287 ms | 15 988 ms | 208 MB |
| 4096 | 12 | 12 | 1 200 s | 9 263 ms | — | 820 MB |
| 8192 | 13 | 13 | 1 300 s | 40 615 ms | — | 3 268 MB |
| **10 000** | **14** | **14** | 1 400 s | **101 584 ms** | ~400 s *(derived)* | **3 269 MB** |
| 12 000 | 14 | 14 | 1 400 s | 149 001 ms | — | 6 532 MB |

Four facts the decision needs:

1. **Convergence hits the ⌈log₂N⌉ bound at every N tested, up to 12 000.** The strategy is not the
   scale bottleneck. O(N²) *representation* is.
2. **Debug is ~10× release** (1024: 4.0 s vs 0.40 s; 2048: 16.0 s vs 5.3 s). `cargo nextest` gates
   **unoptimized**, so per-PR budgets must be set on the debug column.
3. **Cost is O(N²), because `NetworkProfile` materializes N·(N−1) *directed* edges.**
   `BandwidthDistribution::populate` (`fixtures.rs:95-106`) inserts every ordered pair. This is a
   representation cost, not a cascade cost.
4. **RSS advances in power-of-two HashMap rehash steps, not smoothly** (55 → 820 → 3268 → 6532 MB).
   N=8192 and N=10 000 land in the same bucket at 3 268 MB; N=12 000 jumps to 6 532 MB. Budget
   against the *next* power of two, not the measured value, or a 30 % N increase doubles memory.
   **The N=10 000 RSS reading is approximate** and should not be read as a trend point: it sits below
   the N=12 000 figure on an O(N²) curve, which cannot be exactly right. CON-18 measured the same
   anomaly independently and flagged it the same way. Wall time, by contrast, is consistent across
   both measurement runs — **size memory against the power of two, not against N=10 000's reading.**

**Independent re-measurement (CON-18, `e548a03`).** The sweep was repeated in a second environment and
lands within ~15 % on wall time, with the same ordering and the same practical conclusion:

| N | 64 | 256 | 1024 | 2048 | 4096 | 8192 | 10000 | 12288 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| wall, this table | 1.2 ms | 82 ms | 404 ms | 5.3 s | 9.3 s | 40.6 s | **102 s** | 149 s |
| wall, CON-18 | 1.4 ms | 21 ms | 430 ms | 2.6 s | 10.8 s | 50 s | **87 s** | 147 s |

Both runs agree on the two load-bearing conclusions — **N=10 000 costs ~90–100 s and ~3.3 GB in
release**, and **the practical per-PR ceiling is N ≈ 4096** (~10 s / 820 MB), with N ≤ 1024
comfortable at ~0.4 s / 55 MB. N=256 is the one visible divergence (21 ms vs 82 ms), which is
process-startup noise at that scale and changes nothing. The cascade hit `⌈log₂N⌉` at every N in
both runs.

**Sizing verdict for the N=10k question:** N=10 000 is **achievable today** in release — 100 s,
3.3 GB, correct answer. It is **not** achievable as a blocking per-PR gate in debug (~400 s derived,
and nextest's `terminate-after = 10` × `period = 30s` **hard-kills any test at 300 s** — see
`.config/nextest.toml`). N=10k is a nightly/weekly sizing job. **Target for a future sparse or
topology-generative `NetworkProfile`:** N=10 000 in <10 s wall and <512 MB, which would move it
into the nightly-blocking tier. That is a target, not current capability.

### 3.3 CI cost envelope

Measured against `.github/workflows/ci.yml` as it exists today. Test-time only; add build time on top.

| Tier | Contents | Test wall | Runner-min | Gate |
|---|---|---:|---|---|
| **PR-blocking (must)** | nextest `--profile ci`, N ≤ 1024, `fail-fast = false` | ~111 s serial, **less in parallel** | **~6** | blocks merge |
| **PR-advisory** | scale 2048, higher-loss variants | +30 s | +1 | warn |
| **Nightly (blocking)** | scale 4096, calibration vs containers | ~5 min | **~7** | blocks merge to `main` |
| **Weekly sizing** | N=8192 / 10 000 / 12 000 sweep | ~5 min | **~7** | report only |

The 111 s is a **libtest serial** figure. libtest runs each target's tests one at a time; nextest
runs each test in its own process across all cores, so nextest's wall for the same suite is
**substantially lower** — plausibly ~30–40 s, since 94 % of the time sits in two files whose tests are
independent. **That is a derived estimate, not a measurement** (`cargo nextest` is not installed in the
measurement environment). The runner-minute and dollar budgets below are sized against the *serial*
figure deliberately, so they are conservative; re-measure with nextest before quoting a final number.

**Hard constraints any candidate must fit:**

- **≤300 s per test** (nextest `terminate-after = 10` × 30 s `period`). A scenario that violates this
  does not fail — it is *killed*, and the failure is indistinguishable from a hang.
  *The current suite is 37 % of the way to that line in aggregate, and 8 % on its worst single test.*
- **≤30 s per test** to avoid the slow-test warning; **≤2 GB RSS** per test process to coexist with
  a cargo build on a standard 4-vCPU / 16 GB GitHub-hosted runner. `several_dead_nodes_still_orphan_reroot_at_scale`
  is already at 23 s of the 30 s budget — **one more scale added to `SCALES` breaks it.**
- **≤10 runner-minutes added to the PR lane.** At GitHub's published Linux rate (~$0.008/min on
  2-core, private repos; $0 on public repos) the PR lane stays under **$0.03/run**. Confirm the
  current rate before quoting a figure — the durable unit is **runner-minutes**, not dollars.
- **The existing `ci.yml` cannot enforce any of this.** All six test steps are failure-swallowed
  (`2>&1 || true` / `continue-on-error: true`); only `cargo fmt --check` and `cargo doc` can fail the
  workflow. **Removing those guards (CON-17 gap G0) is a prerequisite for every threshold in this
  document.** Until it lands, every pass mark below is unverifiable in CI and enforced only locally.
  Note the asymmetry this creates: the suite got 2.5× more expensive while remaining non-gating.


---

## 4. The rubric

Six rows. Each is scored **0–3** against the stated evidence. Scores must cite an artifact — a test
name, a command, or a file:line. A row with no artifact scores 0.

| Score | Meaning |
|---|---|
| **0** | Absent, or present but unmeasurable |
| **1** | Demonstrated by a non-gating script or example |
| **2** | Demonstrated by an asserted test, but not wired into a gating CI lane |
| **3** | Asserted test **and** gating in CI, with the artifact published as a build output |

### 4.1 Fidelity

*Can the framework reproduce real `nix copy` / SSH / cascade behaviour on an HPC fabric?*

Capability already in the model (score against what is *asserted*, not what is *possible*):

| Dimension | Representation | Location | Asserted today? |
|---|---|---|---|
| Bandwidth, per directed edge | `HashMap<(NodeId,NodeId),u64>` | `cascade.rs:144` | partially — `contention.rs` |
| Asymmetric links | separate `uplink` / `downlink` per node | `NodeSpec`, `cascade.rs:121-126` | **yes** — `contention.rs` |
| Latency, per directed edge | `HashMap<(NodeId,NodeId),Duration>` | `cascade.rs:146` | partially |
| Hard partition | `HashSet<(NodeId,NodeId)>` + `is_partitioned` | `cascade.rs:147,163` | **yes** — `partition_returns_partitioned_error` |
| Mid-flight partition | `FailureSchedule::PartitionAtRound{src,tgt,round}` | `fixtures.rs:223` | **yes** — `fuzz.rs:164` drives it; `s3_never_reports_a_partition_as_success` (`rubric_scenarios.rs`) asserts the safety property |
| Node failure | `KillNodeAtRound{node,round}` | `fixtures.rs:221` | **yes** — `error_tree_shape_under_killed_node`, plus `scale_failures.rs` at 64/256/1024 |
| Stochastic failure | `Random{fraction,seed}` (hash of seed+round+src+tgt) | `fixtures.rs:228-235` | no |
| Peer-SSH denied | fleet variant → transient `CascadeError::Copy` | `fixtures.rs:237+` | **yes** — `peer_ssh.rs` |
| Packet loss | `LinkModel::PacketLoss::{PerChunk,Degrading}` + MSS/RTT slow-start | `src/link.rs` (committed, `f3acd8e`) | **yes** — `scale_failures.rs:185,209` at 5 % per-chunk; asserts *timing inflation, not round-count change* |
| Latency jitter | `Jitter::sample` — seeded per-edge relative spread | `src/link.rs` | **yes** — `deploy_dag.rs` (`.with_jitter(0.2)`) and 13 inline unit tests |
| Asymmetric contention | `LinkDirection::{Uplink,Downlink}` sharing model | `src/link.rs` | **yes** — `deploy_dag.rs:321` `asymmetric_uplinks_are_actually_charged` |
| Deploy-DAG (eval→build→copy→health→activate) | `dag_sim::{DeployDagSim,StageSchedule,StageCost}` | `src/dag_sim.rs` (committed, `f3acd8e`) | **yes** — 12 tests in `deploy_dag.rs`, at 64/256/1024 |

> `src/link.rs` and `src/dag_sim.rs` were **uncommitted** when this rubric was first drafted. They are
> now committed at `f3acd8e`, green, and asserted — so they score **2** (asserted, not yet in a
> gating lane), not 1. This is the single largest score movement since the first draft: F2 and F3
> went from "modelled, unasserted" to "asserted."
>
> The remaining hard gap in this table is **F4, calibration** — `consortium-nix/src/calibration.rs` is
> still **untracked** (`git ls-files` confirms) and nothing compares sim against real nodes.

**One assertion is worth reading closely before it is relied on.**
`a_lossy_fabric_converges_in_the_same_number_of_rounds` (`scale_failures.rs:199`) asserts
`lossy.rounds == clean.rounds` **and** `lossy_total > clean_total`. That is a correct and useful
property — loss is a *cost* effect, not a *topology* effect — but it also means **S3's round-count
threshold is already structurally satisfied** and is a weaker gate than it appears. The safety
property in S3 (never report a node converged without a live path) is the part that is *not* asserted.


**Thresholds.** Fidelity ≥ **2** requires, in asserted tests:

- F1 — mid-flight partition (`PartitionAtRound`) re-routes rather than aborts: cascade converges or
  fails with a correct error tree, and never reports a node converged without a live path.
  **Status: met** — as of `e548a03` this is no longer the row's weakest point. S3
  (`rubric_scenarios.rs`) asserts the no-false-convergence half directly: at N=256 with 5 % loss and
  a mid-flight partition, 255 nodes converge via surviving paths and the 256th is reported
  `Partitioned`, not converged. Previously this half was only driven in `fuzz.rs` under generic
  sanity bounds.
- F2 — `Random{fraction}` at ≥5 % loss still converges with a bounded round count, and is
  **bit-reproducible from its seed**. **Status: met for per-chunk loss** (`scale_failures.rs`, 5 %,
  reproducible at 3 scales) — note this is `LinkModel`, a different mechanism from the
  `FailureSchedule::Random` node-failure draw, which is still unasserted.
- F3 — asymmetric links are modelled: an uplink-constrained node is never credited downlink
  capacity, and per-edge `bandwidth` overrides per-node `NodeSpec` where both are set.
  **Status: met** — `deploy_dag.rs:321`.
- F4 — **calibration**: a sim run and a containerized run of the *same* scenario agree within a
  stated tolerance (see S4). This is the only row that can distinguish "plausible" from "faithful".

**F4 is the load-bearing requirement and it is still unbuilt.** `consortium-nix/src/calibration.rs`
remains untracked and nothing compares tiers yet (CON-17 gap G3). Three of four fidelity sub-rows
are now asserted, which makes F4 *more* load-bearing, not less: the simulator now makes confident
predictions about loss, jitter, and asymmetry, and none of them have ever been checked against a real
node. A framework that scores 3 on F1–F3 and 0 on F4 is **not** higher fidelity; it is a simulator
that has never been checked against reality.


### 4.2 Scale ceiling

Targets, with the measured cost each implies:

| Tier | Sim N | Real nodes | Rationale |
|---|---|---|---|
| PR-blocking | 256 – 1 024 | 0 | 1 024 costs 4.8 s debug / 55 MB; `SCALES = [64,256,1024]` is what every current test uses |
| PR-advisory | 2 048 | 0 | 16.0 s debug — warns, does not block |
| Nightly | 4 096 | 25 + 5 GPU + 2 login + 1 controller | 9.3 s release; containerized cluster is the existing `start_default()` size |
| Weekly sizing | 8 192 – 12 000 | 0 | report-only; RSS steps in powers of two |
| **Requirement** | **10 000** | **~100** | **measured achievable: ~90–100 s, 3.3 GB, release** — independently re-measured at `e548a03` (§3.2) |

> **The asserted ceiling is 1024, and it is asserted three times over.** `scale_smoke.rs`,
> `scale_failures.rs`, and `deploy_dag.rs` all pin `SCALES = [64, 256, 1024]`. N=10 000 has been
> *measured* (§3.2) but is not covered by a single test. Note also that adding a fourth scale to
> `SCALES` is not free: it multiplies the runtime of the 3-scale sweeps, and
> `several_dead_nodes_still_orphan_reroot_at_scale` is already at 23 s against a 30 s budget (§3.3).
>
> **The practical per-PR ceiling is N ≈ 4096, not 10 000**, and this is a *cost-model* limit, not a
> cascade limit. Both measurement runs hit `⌈log₂N⌉` at every N up to 12 288; what blocks 10k is that
> `NetworkProfile` materialises N·(N−1) directed edges. If the rubric keeps an N=10 000 requirement,
> the honest framing is **"requires a sparse or topology-aware `NetworkProfile`, or an off-PR
> schedule"** — not "the cascade does not scale."

Required topology variants — a framework must cover all four or declare which it cannot:

1. **Homogeneous** — uniform bandwidth, no uplink caps. (Today: `scale_smoke.rs`.)
2. **Heterogeneous** — bimodal bandwidth + bimodal uplinks. (Today: `scale_failures.rs`, `deploy_dag.rs`,
   `contention.rs`; all at 30 % fast fraction, 0.3 loss, 0.2 jitter — one canonical "realistic" fleet.)
3. **Multi-zone** — ≥2 zones with an inter-zone bottleneck, exercising cross-zone re-rooting.
   **Not present in any test**, and S2 must not be counted as covering it: after its inert bimodal
   distribution was removed (§5 S2), S2 is a *uniform-uplink* fleet with no zone structure.
   `asymmetric_uplinks_are_actually_charged` is the nearest thing and is not a zone model. **This is
   the largest genuine gap in the topology matrix.**
4. **Degraded** — partition or loss overlaid on any of the above. Loss: **yes** (`scale_failures.rs`).
   Mid-flight partition: **now asserted** — `s3_never_reports_a_partition_as_success`
   (`rubric_scenarios.rs`, §5 S3).


**Real-fleet ceiling is ~100 nodes** and is a *provisioning* question, not a simulation one:
ephemeral real-fleet provisioning is **unbuilt** — no code in tree. `consortium-test-harness` starts a
33-container cluster today; ~100 nodes is a target requiring a provisioning change (and
`Dockerfile.slurm-controller` is currently orphaned, gap G8).

### 4.3 Determinism / reproducibility

Do **not** score a virtual clock as existing capability. Score the contract in §2.

| # | Requirement | Status |
|---|---|---|
| D1 | Byte-identical `CascadeResult` for fixed seed | **asserted** — `scenario_is_deterministic_in_seed` (`fuzz.rs:259`), and `failed_runs_stay_reproducible_at_scale` (`scale_failures.rs:226`, at 3 scales) |
| D1b | Byte-identical **report artifact** for fixed seed | **asserted** since `e548a03` — `the_report_is_byte_identical_across_repeated_runs_of_one_seed` (`report_schema.rs:78`), over `to_json` *and* `to_markdown`. This is the row that makes a stored report usable as a regression baseline (O3) |
| D2 | Every failure draw is a pure function of `(seed, round, src, tgt)` — no RNG state leakage between edges | **asserted by construction** (`fixtures.rs:228-235`); not independently tested |
| D3 | Jitter/loss draws seeded per-edge, so adding a node does not perturb a sibling's timing | **asserted** — `Jitter::sample` (`link.rs:143`) is a pure fn of `(seed, src, tgt)`, and `reports_are_reproducible_at_every_scale` (`deploy_dag.rs:105`) proves it at 3 scales. Promoted from target at `f3acd8e` |
| D4 | Every scenario is addressable by a single stable id (seed + N + profile) that can be pasted into a bug report | **target** — still no scenario registry; the three "realistic fleet" configs are hand-duplicated across `deploy_dag.rs` and `scale_failures.rs` with no shared constant |
| D5 | A failing seed can be bisected: N re-runs of the scenario isolate the responsible draw | **blocked by D4** |
| D6 | A scenario proves its injected fault actually fired (see the hazard in §5 S3) | **now a hard rule for this document.** `FailureSchedule` only fails edges the strategy plans, so a guessed edge is a silent no-op and the test passes vacuously |


**Do not conflate** `nextest` `retries = 2` with determinism. It hides flakes, and with
`fail-fast = false` it can turn a real regression into a green build. Retries are acceptable for the
containerized tier (real SSH, real timing) and **not** for the deterministic tier, where a repeat
failure is a bug.

**Cost line, not capability:** adopting `madsim` for true virtual time would be a genuine upgrade for
D3–D5 (it also enables time-travel debugging and would make the sim's cost independent of modelled
fleet time). It is a rewrite of `consortium-fanout-sim`'s core, and must be scored as **migration
cost against extending what exists** — not assumed for free.

### 4.4 CI cost

Measured envelope in §3.3. Score against the runner-minute total, not the dollar figure.

- C1 — PR lane addition ≤ **10 runner-min** and ≤ **$0.03** at current GitHub Linux rates.
- C2 — no single test > **300 s** (nextest hard kill) or > **2 GB RSS**.
- C3 — nightly lane ≤ **20 runner-min**; weekly sizing ≤ **20 runner-min**.
- C4 — cost **scaling exponent documented**, not just end-point cost. Today: O(N²) in both time and
  memory, and the O(N²) is representational (`N·(N-1)` directed edges), not algorithmic. A candidate
  that is O(N²) *by representation* should be marked as such, not credited with "it scales".

### 4.5 Observability

Required structured metrics, per scenario run:

| Metric | Exists? |
|---|---|
| fan-out depth | **emitted** — `RunReport::fan_out_depth` (`report.rs:247`) |
| time-to-converge | **emitted** — `RunReport::time_to_converge` (`report.rs:253`), plus `fan_out_width`, `rounds` |
| per-edge throughput | **emitted** — `RunReport::edge_throughput` (`report.rs:257`), every successful edge, sorted by `(src, tgt)` |
| failure subtrees | **emitted** — `RunReport::failure_subtree` (`report.rs:259`) |

**All four were "derivable but not emitted" at `f3acd8e`. All four are emitted at `e548a03`.** The
first draft of this rubric said the shape existed only in the uncommitted `calibration.rs`; that is no
longer the target shape — `RunReport` *is* the target shape, and it is the better of the two, because
it is asserted (10 tests in `report_schema.rs`) and version-stamped rather than merely sketched.
`dag_sim.rs` extends it with `phase_timings` and `deploy_wall_time` for the deploy-DAG tier.

What remains:

- O1 — a **versioned JSON artifact** per run: `{version, scenario_id, seed, n_nodes, profile, metrics}`
  written to a stable path. Consumers reject unknown versions rather than guessing. **Status:
  `RunReport::SCHEMA_VERSION: u32 = 1` (`report.rs:60`) with `schema_version` stamped into every
  emitted document (`report.rs:371,500`) and asserted by `report_schema.rs`.** What is still missing
  is the **stable write path** — `examples/emit_report.rs` writes a `.json` + `.md` pair per scenario
  per scale, but nothing in CI invokes it, and consumers do not yet *reject* unknown versions.
- O2 — the artifact uploaded as a **CI build artifact** on every run, gated or not. **Still
  unbuilt.** The `junit.path` setting in `.config/nextest.toml` shows the upload mechanism is
  already configured for test reports; the same pattern applies here.
- O3 — a **regression hook**: a comparator that diffs this run's metrics against the recorded
  baseline and fails on regression. `autoresearch/perf-baselines/` and
  `autoresearch/scripts/perf-baseline.sh` are the shape; criterion `estimates.json` via
  `compute-baseline.sh:80-92` is the precedent. **Now cheap rather than speculative:** byte-identical
  JSON for a fixed seed (§2) means a stored report diffs with no tolerance window, and no
  canonicalisation step. This is the single biggest reduction in the cost of O3.
- O4 — a **scorecard** in the existing `.github/workflows/migration-scorecard.yml` shape (the one
  working per-PR scorecard in this repo) reporting rubric rows R1–R6, not just pass/fail.

> Note: `compute-baseline.sh` references a `dag_executor` criterion bench that **does not exist**;
> the only real criterion bench is `cascade_strategies`, and it is not run by CI. Any O3 implementation
> must not depend on that dangling path.

### 4.6 Dev ergonomics

The question is narrow and answerable: **a developer reproduces a CI failure locally in one command,
on a clean checkout, without Docker or an HPC allocation.**

| # | Requirement | Status |
|---|---|---|
| E1 — | One documented command runs the full deterministic tier | **target** — `cargo test -p consortium-fanout-sim` is the de-facto answer; it is undocumented |
| E2 — | The failing `scenario_id` from the CI artifact is directly runnable (seed + N + profile) | **target** — needs D4 |
| E3 — | Reproducing a CI failure must not require re-running the whole suite | blocked by D4 |
| E4 — | The containerized tier is runnable on a laptop and is clearly marked as *not* bit-reproducible | **partial** — `DockerCluster::start_small()`, `docker_available()` guard |
| E5 — | A local run and a CI run of the same scenario produce **identical** metrics | target — follows from D1 + O1 |

`compute-baseline.sh` is the reusable precedent (nextest + clippy against master in a temp worktree,
emitting machine-readable JSON). E1–E3 should reuse that shape rather than invent a second one.

---

## 5. Reference scenarios

These four are the baseline lock-in. Each states its **measurement** (so it is reproducible), its
**threshold**, and the threshold's **status** — which is the honest part:

- **[measured]** — asserted by a test that exists and passes today.
- **[derived]** — computed from the measured model; becomes an assertion when adopted.
- **[target]** — a number chosen for the framework to hit; **not** evidence of anything today.

**Adoption status as of `e548a03`: S1, S2, and S3 are all `[measured]` and asserted in
`tests/rubric_scenarios.rs` and `tests/scale_smoke.rs`. S4 remains `[target]` — its simulation half is
asserted at N ≤ 1024, its calibration half is unbuilt.** Two thresholds were **removed** during
adoption rather than kept as aspirational, because they were unsatisfiable (S2, both rows) — see below.
That removal is the point of building the scenarios: a target that the code cannot reach is a defect
in the rubric, and it is cheapest to find before a candidate is eliminated on it.

Rates are given in **bytes/sec** because that is the sim's unit. 50 Mbit/s = 6 250 000 B/s.

### S1 — Baseline convergence (PR-blocking, `[measured]`)

> N=1024 full-mesh cascade, uniform 1 GB/s edges, `MaxBottleneckSpanning`, seed `0xdead_beef_1024`.

| Threshold | Value | Status |
|---|---|---|
| Rounds | `== ⌈log₂1024⌉ == 10` exactly | **[measured]** — `scale_smoke.rs:109` |
| Converged | `== 1024` | **[measured]** |
| Wall (debug) | ≤ 6 s | **[derived]** from 3 988 ms + 50 % headroom |
| Peak RSS | ≤ 256 MB | **[derived]** from 55 MB + headroom |

*This is the existing test. It is the anchor that the rest of the ladder extends.*

### S2 — Asymmetric-link cascade at fabric rates (PR-blocking, `[measured]`)

> N=256, per-node uplink **6 250 000 B/s (50 Mbit/s)**, downlink `4 × uplink`
> (`UplinkDistribution::Uniform` → `NodeSpec { uplink, downlink: 4 × uplink }`, `fixtures.rs:172-181`),
> `MaxBottleneckSpanning`, seed `0x00A2`, 10 MiB closure.
>
> **Two parameters were removed from this scenario after it was built and measured.** Both were inert —
> specified but incapable of affecting the result. See the two rows marked ❌ below. The scenario is
> now built as `tests/rubric_scenarios.rs` (CON-18, `e548a03`), 5 tests, all passing.
>
> **Correction: this is a homogeneous-uplink fleet, not a two-zone one.** The first draft called it
> "two zones" because the bimodal edge bandwidth *sounded* like zoning. It is not. After the inert
> distribution was removed, **S2 has no zone structure at all** — all 256 nodes share one uplink. It
> therefore does **not** satisfy topology variant 3 (multi-zone) in §4.2, which remains genuinely
> uncovered. The heterogeneity S2 *does* exercise is the 1:4 uplink/downlink **asymmetry**, which is
> observable and is what the F3 row measures.

| Threshold | Value | Status |
|---|---|---|
| Converged | `== 256` | **[measured]** — 256/256 |
| Fan-out depth | `≤ ⌈log₂256⌉ == 8` | **[measured]** — exactly 8, hits the bound |
| Time-to-converge | `≤ 8 × (10 MiB closure ÷ 6.25 MB/s + 1 ms latency) ≈ 13.5 s` modelled | **[measured]** — 13.45 s; the closed form holds as written |
| ~~Per-node credit~~ | ~~no node credited more than its `uplink`~~ | ❌ **removed — unsatisfiable by the cascade**, re-scoped to the DAG below |
| ~~30/70 bimodal edge bandwidth~~ | ~~heterogeneous edges~~ | ❌ **removed — inert** under this scenario's uniform uplink; heterogeneity preserved via asymmetric `uplink`/`downlink` |
| Wall (debug) | ≤ 10 s | **[measured]** — 0.30 s, 11 MB. Generous, as predicted |
| Peak RSS | ≤ 256 MB | **[measured]** — 11 MB |

#### ❌ The removed "per-node credit" row, and where contention actually lives

The first draft required that *no node be credited more than its `uplink`*, and that an uplink-bound
round duration be the max over that node's out-edges. **The cascade cannot exercise this, by
construction.** `CascadePlan::assignments` documents that *"each source must appear at most once"*
(`cascade.rs:211`), and the strategy comment at `cascade.rs:998` repeats it as the reason convergence
is logarithmic. A source therefore never has more than one out-edge in a round, so
`LinkModel::effective_bandwidth`'s `src_uplink / src_out_degree.max(1)` is *always* `uplink / 1`, and
`shared` contention is **structurally impossible** — the threshold could be neither met nor broken.
Measured: max out-edges per source per round = 1, at every round.

This is not a defect. It is the plan invariant that buys the ⌈log₂N⌉ round bound. But it means the
original row asked the cascade to prove something about a code path the cascade does not have, and a
candidate could have been marked down for failing an unreachable threshold.

**Re-scoped:** uplink contention is scored against the **deploy DAG's parallel `copy`**, which does
divide one uplink K ways and is where `peak_concurrency` in the phase timings comes from. The
scenario that carries this threshold is **S4**, and the property that demonstrates it is
`a_saturated_source_uplink_caps_what_parallelism_can_buy` (`deploy_dag.rs`). F3 in §4.1 is scored
there, not here.

#### ❌ The removed bimodal edge bandwidth

The first draft specified 30/70 bimodal **edge** bandwidth (`slow: 6 250 000`, `fast: 1 GiB`,
`fast_fraction: 0.3`) on top of a uniform 6 250 000 B/s uplink. Effective bandwidth is
`edge_bandwidth.min(shared)` (`link.rs:253`), and **both tiers resolve to exactly 6 250 000 B/s** — the
slow tier by equality, the fast tier by being clamped *down* to the uplink. So all 255 copy edges
collapse to **one** duration (1.6812216 s each) and the intended heterogeneity is invisible. Specifying
a distribution the clamp then erases reads as coverage while testing nothing.

**The heterogeneity this scenario is meant to exercise is preserved** via the asymmetric per-node
`uplink`/`downlink`, which *is* observable and *is* what the F3 row measures. The inert edge-bandwidth
parameter is dropped rather than raised above the uplink: raising the slow tier above 6 250 000 B/s
would make edge bandwidth the binding constraint and quietly convert the scenario into a different
one. An assertion now pins the inert behaviour
(`s2_bimodal_edge_bandwidth_is_inert_under_a_uniform_uplink`), so a future change to the contention
model trips a test instead of passing silently.

### S3 — Packet loss + mid-flight partition (PR-blocking, `[measured]`)

> N=256, **5 % per-chunk loss**, plus one subtree partitioned **mid-flight** at round 4, seed fixed.
> Built and measured as `tests/rubric_scenarios.rs` (CON-18, `e548a03`), 5 tests, all passing.

| Threshold | Value | Status |
|---|---|---|
| Completes | terminates within `≤ ⌈log₂256⌉ + 4 == 12` rounds | **[measured]** — **8 rounds**. The partition costs no extra rounds. See the caveat below |
| Orphans re-rooted | `≥ 99 %` (255/256) of nodes cut off by the partition complete via a surviving path | **[measured]** — 255/256 = **99.6 %**. The brief's "Z %", now frozen at the measured value |
| False convergence | **zero** — any node reported converged must have a live path; a partition is never reported as success | **[measured]** — holds; 255 converged, the 256th is named `Partitioned` |
| Error tree | a single partition yields a single leaf `Partitioned` error naming the edge | **[measured]** — asserted. *Depth-ordered multi-leaf bubbling is asserted elsewhere, not here* — see below |
| Reproducible | same seed → byte-identical converged set and error tree | **[measured]** — `s3_is_byte_for_byte_reproducible` |

**The 12-round bound is weaker than it looks.** `scale_failures.rs:199` already asserts
`lossy.rounds == clean.rounds` — loss is a *cost* effect, not a *topology* effect — so at N ≤ 1024 the
bound is structurally satisfied. The measured 8 rounds confirm this. **The safety property (zero
false convergence) is the part that carries this scenario**, and it is the part that was previously
unasserted for partitions.

**On bubbling — S3 does not cover it.** A *single* partition produces a *single* leaf error.
`SubtreeAggregate` nesting only appears when a parent has more than one failed child, which
`correctness.rs` already covers for `KillNodeAtRound` and `scale_failures.rs` covers at scale. Stating
this here so S3 is not read as covering the depth-ordering property.

#### ⚠️ Scenario-authoring hazard: faults that never fire produce green tests

`FailureSchedule` can only fail edges the strategy **actually plans**. A guessed `(src, tgt)` is
therefore silently a no-op: the run succeeds, the test passes, and it has proven nothing. The natural
guess `(0, 3)` is never planned — node 3's parent is node 1.

CON-18's first draft of S3 passed **vacuously** this way. The fix is to discover the partition target
from a clean traced run rather than guess it. **Any scenario added to this file must assert that its
injected fault actually fired** — by checking the error tree names the intended edge, or the converged
set is strictly short. A scenario that cannot show its fault fired is not evidence.


### S4 — Deploy-DAG simulation + calibration vs real nodes (nightly, `[target]`)

> N=4096, full `eval → build → copy → health → activate` DAG, seeded identically to S1. Then the
> **same** scenario run against the containerized cluster and compared.

| Threshold | Value | Status |
|---|---|---|
| DAG completes | all N nodes reach `activate` | **[measured]** at N ≤ 1024 — `every_scale_converges_completely_on_a_realistic_fleet` (`deploy_dag.rs:62`). **[target]** at 4096 |
| Wall (release) | ≤ 60 s at 4096 | **[target]** — cascade-only 4096 is 9.3 s; the DAG is ~6× the work |
| Peak RSS | ≤ 4 GB | **[target]** — 4096 cascade is 820 MB; 6 532 MB is one power-of-two step up |
| Round depth | DAG converges in `≤ 3 × ⌈log₂4096⌉ == 36` stages | **[target]** — `per_stage_timing_is_monotone_in_the_dependency_chain` asserts ordering, not the bound |
| **Calibration** | sim vs container agree on **converged set** exactly, and on time-to-converge within **±25 %** | **[target]** — still zero code |

**The deploy-DAG simulator now exists and is well-tested; only the calibration is missing.** This is
the most consequential change since the first draft. `dag_sim.rs` was uncommitted and 12 tests
against it now pass, covering exactly the properties S4 asks for *on the sim side*: full convergence,
real-executor completeness, reproducibility, stage ordering, copy dominance, parallelism saturation
(`a_saturated_source_uplink_caps_what_parallelism_can_buy` — the uplink-constrained-credit property
**re-scoped here from S2**, §5), failure isolation, loss inflation, asymmetric uplink charging, and
jitter not changing convergence.

So S4's honest scoping is now:
- **Built and asserted:** the DAG half, at N ≤ 1024, on a canonical "realistic" fleet
  (3 % loss, 0.2 jitter, bimodal 30 % fast, uplink-limited).
- **Unbuilt:** the calibration half. `calibration.rs` is untracked, and the shared tree does not
  compile. Sim and container have still never been run against the same scenario.

**The ±25 % tolerance is still a starting hypothesis, not a measured fact.** It must be calibrated
against the first real sim-vs-container comparison and then frozen. If sim and container cannot be
brought within a stated tolerance, that is the single most important finding the framework produces —
it means the simulator is not fit for capacity planning, whatever its test count. The existence of 12
green DAG tests does not move this: it raises the cost of being wrong, because the simulator now
looks more credible while remaining unvalidated.


---

## 6. Scoring procedure

1. Each candidate is scored 0–3 on R1–R6 (§4). **Every score cites an artifact.** No artifact → 0.
2. The matrix is filled **before** discussing cost, so the cheapest-looking option does not set the
   frame. R1's F4 sub-score is reported separately from R1's others.
3. Ties are broken by **lowest total runner-minutes in the PR lane**, then by smallest migration
   surface. Both are objective.
4. A candidate that cannot satisfy S1 and S2 is eliminated. S3 and S4 decide between survivors.
5. If no candidate satisfies S4's calibration, the framework decision must record that the simulator
   is unvalidated and scope every claim made on its output accordingly.

### Anti-patterns this rubric is designed to prevent

| Anti-pattern | Why the rubric prevents it |
|---|---|
| Scoring a candidate for `madsim` virtual time | §2 — no `madsim` exists; it is priced as migration cost |
| "It scales to 10k" | §3.2 — 10k is ~90–100 s / 3.3 GB release, ~400 s debug, killed by nextest at 300 s |
| Equating modelled fleet time with wall-time | §3.2 — two different quantities, both reported |
| Citing green tests as evidence of a gate | §3.3 — `ci.yml` swallows all six test failures; G0 is a prerequisite |
| Counting `retries = 2` as stability | §4.3 — retries hide deterministic-tier regressions |
| **A threshold the code cannot reach** | §5 S2 — a candidate was one scoring pass away from being eliminated for failing a property the cascade structurally cannot exercise. Every threshold must be checked against the invariant that governs it (`CascadePlan`: one source per round) before it is written down |
| **A fault that never fires** | §5 S3, D6 — `FailureSchedule` only fails *planned* edges; a guessed `(src, tgt)` makes the run succeed and the test pass vacuously |
| **A distribution the clamp erases** | §5 S2 — bimodal edge bandwidth under a uniform uplink collapses to one duration; it reads as coverage while testing nothing |
| Quoting a test count without a SHA | §3.1 — 35 → 89 → 105 → 115 in three days; only the commit makes a count meaningful |
| Citing `parallel_builds.rs` as a bench pattern | It is a 3.7 KB stub with no criterion and no execution (CON-17 §2.4) |
| Citing the 37 filled `TEST_MAPPING.toml` entries as coverage | 6.4 %, `range_set`/`node_set` only, partly name-duplicated — an upper bound |

---

## 7. Open questions for the framework decision

These are decisions, not gaps in the rubric. Each needs an owner.

1. **Sparse or dense `NetworkProfile`?** The O(N²) directed-edge map is the sole reason N=10k costs
   3.3 GB. A topology-generative model (sample edges on demand from a seeded topology) would make
   N=10k cheap and is the highest-leverage single change on this list. *Owner: whoever owns S1's
   extension.*
2. **Adopt `madsim`, or keep time-as-data?** Keeping time-as-data is cheaper and already
   deterministic. `madsim` buys D3–D5. Decide explicitly rather than by default.
3. **Where does the 4096+ tier live?** `consortium-test-harness` (real) or `fanout-sim` (modelled)?
   S4 says both, which means the calibration harness has to exist first.
4. **Is `consortium-testing` a real repo?** The original brief targeted it; it is **empty** (no
   commits, no clone) and all assets live in `consortium-autoresearch`. Repo provisioning is a
   question for [DevOps Engineer](/CON/agents/devops-engineer), and it is **not** a prerequisite for
   this rubric.
5. **Who removes the `|| true` guards in `ci.yml` (G0)?** Nothing in this document is enforceable in
   CI until that lands.

---

## 8. Reproducing §3

```bash
git clone https://github.com/olivecasazza/consortium-autoresearch && cd consortium-autoresearch
git checkout f3acd8e                      # clean tree; the shared working tree does not compile
                                          # §5 S2/S3 measurements are on CON-18's local branch
                                          # feat/con-18-report-schema @ e548a03 — not pushed

export PATH="$HOME/.cargo/bin:$PATH"

# §3.1 — suite timing (per-target 'finished in' figures)
cargo test -p consortium-fanout-sim

# §3.2 — scale-ceiling sweep (release); --isolate is the default
cargo run --release --example scale_sizing -p consortium-fanout-sim
cargo run --release --example scale_sizing -p consortium-fanout-sim -- 10000

# §3.2 — debug column (this is what a nextest gate actually pays)
cargo run -p consortium-fanout-sim --example scale_sizing -- 1024 2048

# §3.2 — independent peak-RSS measurement (the in-process VmHWM reading is
# quantised by HashMap rehash steps, so verify externally)
python3 -c "
import resource,subprocess
for n in [1024,4096,8192,10000,12000]:
    subprocess.run(['./target/release/examples/scale_sizing',str(n),'--in-process'],
                   capture_output=True)
    print(n, resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss/1024, 'MB')"

# §5 — the rubric's own reference scenarios (CON-18, e548a03)
cargo test -p consortium-fanout-sim --test rubric_scenarios
cargo test -p consortium-fanout-sim --test report_schema

# §4.5 — emit the versioned report artifact (O1)
cargo run --release --example emit_report -p consortium-fanout-sim
```

**Two things to know before re-measuring.** `cargo nextest` is not installed in the measurement
environment, so §3.1 was taken with `cargo test` (libtest). libtest runs each target's tests serially;
nextest parallelises across processes, so nextest's wall-time for the same suite will be **lower**,
while its per-test terminate-after still applies. Re-measure with nextest before quoting a PR-lane
budget as final.

**A full clean build of this workspace exceeds a 20-minute budget.** `consortium-fanout-sim` pulls in
the whole `consortium` crate, and the shared working tree does not compile. Budget a rebuild
deliberately or work from a warm `target/`; do not start one and then discover the timeout.
