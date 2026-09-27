# ADR 0001 — Consortium test framework: hybrid, simulation-spine

**Status:** Accepted
**Date:** 2026-09-27
**Decision owner:** HPC Systems Engineering ([CON-22](/CON/issues/CON-22))
**Rubric of record:** [`docs/testing-requirements.md`](/CON/issues/CON-15) — CTO-owned, commit `9bd3458`, 699 lines
**Supersedes:** the candidate matrix in [`framework-evaluation`](/CON/issues/CON-16#document-framework-evaluation) rev 2 and the provisional [`adr-draft`](/CON/issues/CON-16#document-adr-draft) rev 2
**Implements:** [CON-18](/CON/issues/CON-18) · [CON-19](/CON/issues/CON-19) · [CON-20](/CON/issues/CON-20) · [CON-21](/CON/issues/CON-21) · [CON-97](/CON/issues/CON-97) · [CON-99](/CON/issues/CON-99)

---

## 0. The decision in one table

| # | Question | Decision |
|---|---|---|
| **D1** | Which framework? | **Hybrid, simulation-spine.** `consortium-fanout-sim` is the spine and the PR gate; `consortium-test-harness` (Docker) is the fidelity witness and calibration counterpart; real-fleet is a scheduled, non-gating ground-truth job. `madsim`/`turmoil`/`shadow` (A2) is **rejected**, and its re-open trigger is now **falsified** (§3.4). |
| **D2** | Where does it live? | **Extend `consortium-autoresearch` in place**, under a containment rule: new framework code lands only in the two fork-local crates and the fork-local top-level dirs (§4). **No new repo.** `consortium-testing` is not provisioned and is not a prerequisite. |
| **D3** | CI integration? | Four lanes — **PR-blocking** (nextest, sim, N ≤ 1024), **PR-advisory** (N = 2048), **nightly-blocking** (N = 4096 + container calibration), **weekly sizing** (N = 8192–12288, report-only). Every one of them is **unenforceable until G0 lands** ([CON-97](/CON/issues/CON-97)). §5. |
| **D4** | Migration path? | Four moves, ordered by dependency: un-swallow CI → promote the sim lane to gating → build calibration → add the container lane. Current state already **is** the target's spine; this is a promotion, not a rewrite. §6. |
| **D5a** | `crates/consortium-test-harness/`? | **Active.** 477 LOC of working `DockerCluster`, already adopted by `crates/consortium/tests/docker_integration.rs:16-19`. Recorded here, not re-litigated. §7.1. |
| **D5b** | Side-by-side execution tests? | **Out of scope for this framework, and must not gate it.** They belong to the upstream-migration track and stay behind [CON-5](/CON/issues/CON-5). §7.2. |

**One binding obligation rides with D1:** the simulator is **unvalidated**. See §8 — this is now a
requirement of the rubric, not a caveat we are choosing to accept.

---

## 1. Context

`consortium` reimplements ClusterShell's fan-out distribution in Rust. It has two test layers that
were built independently and never reconciled:

- `crates/consortium-fanout-sim/` — 5 671 LOC. A deterministic simulator of the cascade with time as
  data. 115 tests. Asserted ceiling N = 1024, pinned three times over (`SCALES = [64, 256, 1024]`).
- `crates/consortium-test-harness/` — 477 LOC. A real `DockerCluster` starting 33 containers over
  real SSH. Adopted by exactly one test file.

Neither layer was gated. The program existed to decide which one to keep. Six precursor issues fed
this decision: [CON-15](/CON/issues/CON-15) (rubric), [CON-16](/CON/issues/CON-16) (evaluation),
[CON-17](/CON/issues/CON-17) (asset inventory), [CON-18](/CON/issues/CON-18) (sim prototype),
[CON-19](/CON/issues/CON-19) (container prototype), [CON-20](/CON/issues/CON-20) (report pipeline),
[CON-21](/CON/issues/CON-21) (perf gate).

[CON-22](/CON/issues/CON-22) was held `blocked` pending CON-15, because the evaluation it rests on was
scored against provisional equal weights. The rubric has since landed. This ADR re-scores against it.

---

## 2. The rubric, and the one question it was supposed to settle

### 2.1 Fidelity is a **weight**. CON-16's "Gate 1" is withdrawn.

[CON-16](/CON/issues/CON-16) §6 built its recommendation on a single stated premise:

> **Gate 1 — fidelity is a requirement, not a trade.** A sole approach must score ≥ 3 on fidelity.
> A1 scores 2, and *no amount of work on A1 can raise it* […] A1 is therefore disqualified *as the
> sole answer* and can only ever be a component.

It flagged this as the one input that could invalidate the evaluation: *"What would flip it: if CON-15
ratifies fidelity as a weighted preference rather than a requirement, A1-alone becomes viable and the
recommendation should be revisited."*

**The rubric ratified it as a weighted preference.** Fidelity is **R1**, one of six rows in §4, scored
**0–3** on the same scale as everything else. The rubric's scoring procedure (§6) contains no fidelity
gate. Its elimination rule is:

> 4. A candidate that cannot satisfy **S1 and S2** is eliminated. **S3 and S4** decide between survivors.

S1 and S2 are **simulation** scenarios — a full-mesh cascade at N = 1024 hitting the `⌈log₂N⌉` bound,
and an asymmetric-link cascade at N = 256. Fidelity is nowhere in the elimination rule. And the
rubric explicitly anticipates the fidelity gap being *live at decision time* rather than treating that
as disqualifying:

> **F4 is the load-bearing requirement and it is still unbuilt.** […] A framework that scores 3 on F1–F3
> and 0 on F4 is **not** higher fidelity; it is a simulator that has never been checked against reality.

"Load-bearing requirement" here means *the row that decides whether the other rows mean anything*. It is
not a pass/fail gate on the candidate.

**Consequence:** the premise CON-16 was blocked on has resolved against it. Gate 1 is withdrawn as
written. The recommendation does **not** flip to A1-first, and §3.2 replaces the argument on grounds
that survive.

### 2.2 What replaces it is stronger

The rubric's own procedure, read literally, is more decisive than the weighting argument was — and it
does not depend on any agent's choice of weights.

- **S1 and S2 are simulator-only properties.** Fan-out depth `== ⌈log₂1024⌉ == 10` and a
  time-to-converge closed form are measurements of the cascade *model*. A 33-container real-SSH
  cluster cannot produce them at any N. So §6.4 **eliminates a container-only framework directly** —
  not for low fidelity, but for failing the elimination scenarios.
- **S4 requires both halves.** §5 S4 is "the same scenario run against the containerized cluster and
  compared", with calibration on the converged set and time-to-converge. A simulator-only framework
  cannot reach it either: there is no container side.
- Therefore **the hybrid is the only composition under which every scenario in §5 is expressible**,
  and §6.4's procedure (survive S1+S2, then S3 and S4 decide) can only be completed by it.

This is a better argument than Gate 1 in one specific way: Gate 1 depended on a *weight* CON-15 could
have set differently. This depends on *what the scenarios measure*, which is now written down and
cannot be re-weighted.

### 2.3 Sensitivity

CON-16 claimed the recommendation was insensitive to re-weighting, because A1 was disqualified by Gate 1
"regardless of weights". **That claim is now void** — A1 is not disqualified by a gate. It is
disqualified by §6.4, which is weight-invariant. So the conclusion survives, but for a different
reason than the one on file. Recording this because the prior claim is the kind of thing a later
reader will cite without checking.

---

## 3. D1 — the chosen framework

### 3.1 Score against the ratified rubric

CON-16 scored 0–5 on six axes with provisional equal weights. The rubric scores **0–3** on R1–R6 and
requires every score to cite an artifact. Re-scored against the rubric's own rubric, the **incumbent
hybrid** stands as the only candidate that is not eliminated:

| Row | Sim spine today | Container tier today | Hybrid (chosen) | Rubric note |
|---|---|---|---|---|
| **R1 Fidelity** | 2 — F1–F3 asserted, **F4 unbuilt** | 2 — real SSH, `netem` absent | 2 | §4.1: F4 is the decider; nothing compares tiers yet |
| **R2 Scale ceiling** | 2 — asserted to 1024, 4096 is `[target]` | 1 — 33 nodes, ~100 is a provisioning gap | 2 | §4.2: multi-zone topology **uncovered by any test** |
| **R3 Determinism** | **3** — byte-identical *result and report* | 1 — real timing, `retries = 2` | **3** | §4.3: `retries` must **not** count as determinism |
| **R4 CI cost** | 3 — ~111 s serial, 37 % of the 300 s kill line | 1 — Docker, per-lane minutes | 3 | §4.4: score runner-minutes, not dollars |
| **R5 Observability** | 2 — all 4 metrics emitted; **O1/O2/O3 unbuilt** | 1 | 2 | §4.5: O1 write path and O2 upload are the gap |
| **R6 Dev ergonomics** | 1 — `cargo test` works, undocumented | 1 — needs Docker | 1 | §4.6: E1–E3 all blocked on D4 (no scenario registry) |

Sum **13/18**. Under CON-16's 0–5 scale this is 3.67 — mid-table, and *below* A1's 4.00.

**The hybrid is not the top-scoring row and this ADR does not pretend otherwise.** It is the only
composition that survives §6.4, and §2.2 says why. The honest summary of the program is: *we are
extending the thing that is already mid-table, because the rubric's scenarios cannot be satisfied by
anything else.*

### 3.2 Composition, in the rubric's own tier language

| Tier | Mechanism | Schedule | Gate | Rubric scenario |
|---|---|---|---|---|
| PR-blocking | `consortium-fanout-sim`, N ≤ 1024 | every PR | **blocks merge** | S1, S2, S3 |
| PR-advisory | sim, N = 2048 | every PR | warns | — |
| Nightly | sim N = 4096 **+** `DockerCluster` calibration | nightly | blocks `main` | S4 |
| Weekly sizing | sim, N = 8192 / 10 000 / 12 288 | weekly | report only | §3.2 sizing |
| Ground truth | real nixlab / GCP fleet | scheduled | **never gates** | §4.2 ~100-node ceiling |

The last row is the answer to the rubric's §7 Q3 — *"Where does the 4096+ tier live? `consortium-test-harness`
(real) or `fanout-sim` (modelled)? S4 says both, which means the calibration harness has to exist first."*
**Both.** That is the whole point of choosing a hybrid.

### 3.3 Why not container-only (B), and why not real-fleet-only (C)

- **B (container-only)** is eliminated by §6.4 on S1/S2 (§2.2). It also cannot reach N = 1024, let
  alone 10 000; §4.2 caps the real fleet at ~100 nodes, and that cap is provisioning, not software.
- **C (real-fleet-only)** is disqualified as a *gate*, not on merit. It needs a self-hosted runner with
  reachable builders, real SSH, real timing, and real nodes. As a PR gate its cost is unbounded and
  its flake rate is uncontrolled. It is the **highest-fidelity** thing available and it is therefore
  the **ground-truth job** — never a merge condition.

### 3.4 `madsim` (A2) — rejected, and the re-open trigger is now falsified

CON-16 rejected A2 behind a falsifiable condition, and named the test:

> Reopen A2 only if, *after* A1's DAG work (G1) and network-model work (G2), a specific CON-15 reference
> scenario still cannot be expressed — concretely, a scenario requiring mid-flight partition
> **combined with** per-edge timeout and retry ordering interacting. Write that scenario first.

**The rubric has since written the scenarios, and S3 was built and measured.**
`tests/rubric_scenarios.rs` @ `e548a03` implements it: 5 tests, all passing, including
`s3_never_reports_a_partition_as_success` and `s3_is_byte_for_byte_reproducible`. S3 is the
mid-flight-partition scenario. It is expressible in the incumbent simulator.

**The trigger is falsified for the scenario the rubric actually ratified.** A2 stays rejected, now on
measured evidence rather than on a cost argument. Its price never changed: breaking a synchronous
`RoundExecutor::dispatch` across **9 implementors** into a workspace's-first async runtime, to obtain
virtual time, when determinism is already `3` and byte-identical.

**What survives of the trigger, stated precisely.** S3 covers mid-flight partition *plus loss*, not
per-edge **timeout-and-retry ordering**. The rubric leaves that open (§7 Q2: *"Adopt `madsim`, or keep
time-as-data?"*). So the residual gap is real but it is a **narrow, named** one, and the rubric's §4.3
prices it as *"a rewrite of `consortium-fanout-sim`'s core, and must be scored as migration cost"*. A2
reopens only if a scenario requiring partition + timeout + retry-ordering interaction cannot be written
against the incumbent. Nobody has written it. It is not on the critical path.

### 3.5 The highest-leverage change is not a framework choice

Rubric §7 Q1: *"Sparse or dense `NetworkProfile`? The O(N²) directed-edge map is the sole reason N=10k
costs 3.3 GB. A topology-generative model […] would be the highest-leverage single change on this
list."*

Agreed, and it is **not** a framework decision — it is a data-structure change inside the chosen
spine. It moves the §4.2 N = 10 000 row from nightly-sizing to nightly-blocking. Tracked inside
[CON-18](/CON/issues/CON-18) scope, not as a new issue.

---

## 4. D2 — where it lives

### 4.1 `consortium-testing` does not exist

The original brief said the framework would go in `github.com/olivecasazza/consortium-testing`,
"already wired". **It is empty.** Verified independently of the CTO's correction:

```
$ git -C <consortium-testing worktree> log --oneline -20
fatal: your current branch 'main' does not have any commits yet
$ git -C <consortium-testing worktree> status -sb
## No commits yet on main...origin/main [gone]
```

No commits, `origin/main` gone. The Paperclip project for this issue still points its workspace at it
(`PAPERCLIP_WORKSPACES_JSON` → `consortium-testing-b3871e04233d`), which is why the misconception
persisted. That workspace wiring should be corrected by DevOps; it is a provisioning question per
rubric §7 Q4 and **is not a prerequisite for this decision**.

The real options are therefore: **extend `consortium-autoresearch` in place** (the default, and what
[CON-18](/CON/issues/CON-18)/[CON-19](/CON/issues/CON-19)/[CON-20](/CON/issues/CON-20)/[CON-21](/CON/issues/CON-21)
already assume) **vs. provision a new repo and migrate**.

### 4.2 Decision: extend in place, under a containment rule

Migrating to a new repo to host a test framework is a larger change than the framework decision
itself, and it would have to carry the sim crate, the harness crate, the CI workflows, the nextest
config, the rubric, and every in-flight branch. Nothing in the evidence asks for that.

**The real constraint, weighed rather than waved away:** `consortium-autoresearch` is a fork tracking
upstream `cea-hpc/ClusterShell` (`UPSTREAM_REF: v1.9.3`, with `sync-upstream-1.10.1` merged at
`d00f92e`). Upstream-merge cost is real, so the decision is not "extend freely" — it is:

> **Containment rule.** New framework code lands only in fork-local surfaces:
> `crates/consortium-fanout-sim/`, `crates/consortium-test-harness/`, `crates/consortium-nix/`
> (entirely fork-local Rust — the Python upstream is `crates/consortium-py/`), `examples/`,
> `.config/`, `.github/workflows/`, and fork-local `docs/`.
> **Never** inside `crates/consortium-py/ClusterShell/`, `doc/`, `tests/*.py`, or `conf/` — the
> 128 files that are actually upstream.

The arithmetic supports this: **261 of 389 tracked files (67 %) are already fork-local.** The framework
lands entirely inside the fork-local 67 % and adds nothing to the upstream diff. `consortium-nix` is
safe to extend despite the constraint — it is fork-local Rust, not vendored Python, which is why G2
(extending `NetworkProfile`) has no upstream-merge cost at all.

### 4.3 ADR location, for the same reason

`consortium-autoresearch` has no ADR convention. **`docs/adr/0001-hybrid-test-framework.md`**, matching
the existing fork-local `docs/` (which holds `testing-requirements.md`). Not `doc/` — that is the
upstream Sphinx tree, and adding to it would put fork-local prose into every future upstream merge.

---

## 5. D3 — CI integration plan

### 5.1 The precondition, stated bluntly

**Nothing in this plan is enforceable until G0 lands** ([CON-97](/CON/issues/CON-97)). Verified at
`f9b7efd`:

| Workflow | `\|\| true` | `continue-on-error: true` | Can a red test fail the run? |
|---|---|---|---|
| `.github/workflows/ci.yml` | 4 (L42, L105, L134, L169) | 2 (L54 clippy, L211 nix self-hosted) | **No** — only `cargo fmt --check` and `cargo doc` |
| `.github/workflows/migration-scorecard.yml` | 4 (L98, L111, L163, L171) | 3 (L72, L88, L102) | **No** |

The suite grew 35 → 89 → **115 tests** and **5.2 s → 111 s** while remaining non-gating. Every pass mark
in this ADR is therefore *locally* true and *CI-unverified* until step 0 completes. The rubric's
anti-pattern list names this directly: *"Citing green tests as evidence of a gate."*

### 5.2 The lanes

| Lane | Config | Budget (rubric §3.3) | Gate | Replaces |
|---|---|---|---|---|
| **PR-blocking** | nextest `--profile ci`, sim crate, N ≤ 1024 | ≤ 10 runner-min added, **≤ 300 s/test**, ≤ 2 GB RSS | blocks merge | the swallowed step at `ci.yml:37-42` |
| **PR-advisory** | sim, N = 2048 (+30 s) | +1 runner-min | warn | nothing — new |
| **Nightly** | sim N = 4096 + `DockerCluster` calibration | ≤ 20 runner-min | blocks `main` | `Docker Integration Tests` (`ci.yml:66`), promoted |
| **Weekly sizing** | N = 8192 / 10 000 / 12 288 | ≤ 20 runner-min | report only | nothing — new |

`ci.yml:66`'s Docker job is *already* the container lane. It is swallowed at L105. Promoting it is
delete-the-guard, not build-something-new.

### 5.3 Per-workflow dispositions

- **`ci.yml`** — host the two sim lanes. Delete the 4 `|| true` and 2 `continue-on-error`. Fix the
  toolchain drift: L23, L74, L134 use `dtolnay/rust-toolchain@stable` while `rust-toolchain.toml` pins
  `1.94.1` — the pin is currently decorative in CI.
- **`tests.yml`** — **no change.** This is upstream ClusterShell's Python suite (`name: ClusterShell
  nosetests`), renamed from `nosetests.yml` upstream at `6f02f8b`. The original brief's reference to
  `nosetests.yml` is stale; the file is `tests.yml`. It is upstream-forked surface and out of scope.
- **`migration-scorecard.yml`** — see §7.2. The framework does not depend on it. Its `continue-on-error`
  guards are G0's business.
- **`perf-gate.yml`** — already exists for [CON-21](/CON/issues/CON-21) (currently `blocked`). Its
  "new regression gate from #7" role is **O3**, and the rubric has made O3 cheap rather than
  speculative: byte-identical JSON for a fixed seed means a stored report diffs with **no tolerance
  window and no canonicalisation step**. Do **not** wire it to `autoresearch/perf-baseline.sh`'s
  criterion path — `compute-baseline.sh:80-92` references a `dag_executor` bench that **does not exist**.
  The only real criterion bench is `cascade_strategies`, and it is not run by CI.

### 5.4 nextest config, and one setting that must change

`.config/nextest.toml` is already correct for the sim tier: `junit.path`, `fail-fast = false`,
`slow-timeout = { period = "30s", terminate-after = 10 }` (the 300 s hard kill), `retries = 2`.

**`retries = 2` must become tier-scoped.** The rubric is unambiguous: *"Do not conflate `retries = 2`
with determinism. It hides flakes, and with `fail-fast = false` it can turn a real regression into a
green build. Retries are acceptable for the containerized tier […] and **not** for the deterministic
tier, where a repeat failure is a bug."* One `[profile.ci]` for both tiers cannot express this; it
needs a second profile.

---

## 6. D4 — migration path

Current state **is** the target's spine. This is a promotion, not a rewrite. Four moves, in dependency
order:

| # | Move | Unblocks | Issue | Est |
|---|---|---|---|---|
| **0** | Un-swallow CI (G0). Delete 8 guards across 2 workflows, fix toolchain pin drift. | *every threshold in §5* | [CON-97](/CON/issues/CON-97) | ~1 h |
| **1** | Correct the two false `madsim` doc comments (`cascade.rs:23,325`). | — | [CON-98](/CON/issues/CON-98) ✅ done | 5 min |
| **2** | Promote the sim lane to PR-blocking nextest; add a retry-free profile; wire S1–S3 as the gate. | R3, R4, E1 | this ADR → follow-up | 1 d |
| **3** | **sim ↔ container calibration** (G3): one scenario expressed in both, assert a divergence bound. | F4, S4, and §8 | [CON-99](/CON/issues/CON-99) | 2–3 d |
| **4** | Ground-truth job: real-fleet provisioning + teardown + assertion harness, scheduled, non-gating. | §4.2 ~100-node ceiling | needs board approval — §9 | 2–3 d |

Steps 2 and 4 are independent. **Step 3 is the one that matters** — it is the measurement that
justifies the word "hybrid". Without it, D is two suites, and §8 applies at full force.

Prerequisites already delivered and not repeated here: `link.rs` + `dag_sim.rs` committed and asserted
(G1, 12 tests); `RunReport` with all four §4.5 metrics emitted (O1's shape); `emit_report.rs` writing
versioned `.json` + `.md` pairs (O1's write path, but **not invoked by CI** — that is O2, unbuilt).

---

## 7. D5 — dispositions

### 7.1 `crates/consortium-test-harness/` — **active**

Recorded, not re-litigated. The evidence: `src/lib.rs` is **477 LOC** of working `DockerCluster`
(`start_small()`, `start_default()`, `docker_available()`), and it is **already adopted** —
`crates/consortium/tests/docker_integration.rs:16-19` imports `ClusterTopology, DockerCluster` and
lazily starts a shared cluster for that whole file.

Its role under D1 is **the fidelity witness**: it is the container half of S4's calibration. The
rubric's §7 Q3 answer — both tiers, in the same scenario — is unimplementable without it.

**Open work on this crate, not a disposition question:**

- `tc netem` shaping in `Dockerfile.ssh-node` — the single highest fidelity-per-day item in the
  program. It is what makes S4's ±25 % a *fabrication* comparison rather than a loopback one.
- Fold CI's hand-rolled keygen/compose out of `ci.yml:88,147` into this crate (one implementation).
- `Dockerfile.slurm-controller` is **orphaned** (rubric gap G8).
- `start_default()`'s 33 containers = the nightly tier's real-node count. The ~100-node ceiling is
  provisioning work, not a code limit.

### 7.2 Side-by-side execution tests — **out of scope, and must not gate the framework**

[CON-5](/CON/issues/CON-5) asks to remove the side-by-side execution tests "once feature tests are
verified against the upstream ClusterShell test suite." That is **upstream-migration work with its own
gates**, and none of them are met. Concretely, the rubric's own anti-pattern table lists:

> **Citing the 37 filled `TEST_MAPPING.toml` entries as coverage** → 6.4 %, `range_set`/`node_set`
> only, partly name-duplicated — an upper bound.

**6.4 % coverage, partly name-duplicated, for one of two core data structures.** The side-by-side
comparison is not evidence about the test framework, and it must not be made a precondition for it.

**The disposition:** side-by-side execution tests are a **parallel track, not a framework
prerequisite**. `migration-scorecard.yml` gets no lane in §5, no gate in §5.2, and no dependency in
§6. Its three `continue-on-error` guards and four `|| true` are cleaned up by G0 as routine hygiene —
*not* because the framework needs the scorecard to pass. When [CON-5](/CON/issues/CON-5) unblocks on
its own terms, removing these suites changes nothing about D1–D4.

---

## 8. Binding obligation: the simulator is unvalidated

The rubric does not merely permit proceeding without calibration — at decision time it **requires**
recording the consequence:

> If no candidate satisfies S4's calibration, the framework decision **must record that the simulator is
> unvalidated and scope every claim made on its output accordingly.** (§6.5)

**This is the most consequential clause in this ADR, and it is a constraint on us, not a caveat we
tolerate.** `consortium-nix/src/calibration.rs` is untracked; sim and container have never been run
against the same scenario. Until [CON-99](/CON/issues/CON-99) lands:

1. **No capacity-planning claim may be made from simulator output.** Not in a PR description, not in a
   release note, not in a sizing recommendation. The simulator models a *cascade*; it has never been
   compared to a real fabric.
2. **The ±25 % tolerance in S4 is a hypothesis, not a measurement.** It must be calibrated against the
   first real comparison and then frozen. If sim and container cannot be brought within a stated
   tolerance, *that is the most important finding this program produces* — it means the simulator is
   unfit for its purpose, whatever its test count.
3. **The test count must never be used as a proxy for validity.** The rubric's warning is precise:
   *"The existence of 12 green DAG tests does not move this: it raises the cost of being wrong,
   because the simulator now looks more credible while remaining unvalidated."*
4. **Every claim in this ADR inherits the caveat.** S1–S3 are `[measured]` — measured *in the
   simulator*. §3.1's R1 = 2 is a statement about the model, not the fabric.

---

## 9. Decisions this ADR cannot make

**Step 4 (real-fleet ground truth) needs the board, not an agent.** It requires real nixlab / GCP
nodes and real spend. Proceeding is a budget decision. Raised as a board approval request alongside
this ADR. The framework decision is **not** blocked on it — the PR and nightly lanes ship without it,
and ground truth remains a scheduled non-gating job throughout.

**`consortium-testing` provisioning** is DevOps', per rubric §7 Q4, and is **not** on this ADR's
critical path. The project workspace pointing at the empty repo should be corrected so the next agent
does not inherit the same false premise.

---

## 10. Alternatives considered

| Alternative | Why not |
|---|---|
| **A1 — sim only** | Highest-scoring row under CON-16's matrix (4.00), and still eliminated: §6.4's S1/S2 are simulator scenarios it passes, but S4 needs a container it does not have. Viable only as the *spine* of D. |
| **A2 — `madsim` / `turmoil` / `shadow`** | Rejected. Determinism already `3` and byte-identical; the re-open trigger (mid-flight partition scenario) has been **written and measured** — §3.4. 9-implementor synchronous trait break to obtain virtual time. |
| **B — container only** | Eliminated by §6.4 on S1/S2. Cannot reach N = 1024. |
| **C — real-fleet only** | Highest fidelity available, disqualified as a *gate*. Becomes the scheduled ground-truth job. |
| **New `consortium-testing` repo** | A larger change than the decision it serves. §4. |

---

## 11. What would reopen this ADR

Falsifiable, in the rubric's own terms:

1. **S4's calibration lands and sim and container disagree beyond any freezable tolerance.** Then the
   simulator is unfit, and D1 must be re-decided with fidelity as a hard gate after all — because by
   then the fallback in §6.5 would be describing a *known* failure rather than a *not-yet-measured*
   one. (§8.2)
2. **A scenario requiring mid-flight partition + per-edge timeout + retry-ordering interaction cannot
   be written against the incumbent.** Write it first. If it cannot be written, A2 earns its cost. (§3.4)
3. **S1 or S2 becomes unsatisfiable** under a future change to the cascade — the same defect class the
   rubric already caught twice in S2, where two thresholds were removed as unsatisfiable. Every new
   threshold gets checked against the `CascadePlan` invariant ("each source must appear at most once")
   before it is written down.
4. **The PR lane cannot fit 10 runner-minutes** once steps 0 and 2 land. Then the spine moves to
   advisory and the PR gate becomes container-based — which §2.2 says the rubric forbids, so this
   would force the decision back to the board.

---

## 12. Verification performed for this ADR

Run in a **clean `git worktree` at `e548a03`** (the shared working tree does not compile; see below),
rust `1.94.1`:

```
$ cargo test -p consortium-fanout-sim --test rubric_scenarios --test report_schema
  report_schema     10 passed; 0 failed   (4.09 s)
  rubric_scenarios  10 passed; 0 failed   (2.12 s)
EXIT=0

$ cargo run --example emit_report -p consortium-fanout-sim
scenario                    hosts  rounds      depth     converge  min MiB/s
clean                         64       6          6       55.03s       10.0
lossy                         64       6          6       56.73s        9.7
one-dead-node                 63       7          5         NaNs       10.0
single-host                    1       0          0         NaNs        0.0
clean                        256       8          8       80.04s       10.0
lossy                        256       8          8       82.51s        9.7
one-dead-node                255       9          7         NaNs       10.0
single-host                    1       0          0         NaNs        0.0
clean                       1024      10         10      100.05s       10.0
lossy                       1024      10         10      103.14s        9.7
one-dead-node               1023      11          9         NaNs       10.0
single-host                    1       0          0         NaNs        0.0
wrote 12 artifact pairs
EXIT=0
```

This demonstrates, end-to-end and from a clean checkout:

- **S2 and S3 are executable and green** — 10/10, including `s3_never_reports_a_partition_as_success`
  and `s3_is_byte_for_byte_reproducible`.
- **The O1 report path works** — 12 versioned `.json` + `.md` pairs emitted at 3 scales. The chain
  scenario → versioned artifact is real. What is missing is only that **CI never invokes it** (O2) and
  that nothing diffs it against a baseline (O3).
- **The spine is the spine** — `rounds` hits `⌈log₂N⌉` exactly at N = 64/256/1024, and the clean and
  lossy runs converge in the **same** round count at every scale. Loss is a cost effect, not a topology
  effect, which is the property S3 depends on.

**Not verified here, and not claimed:** the container tier (no Docker in this environment), S4
calibration, and every CI gate — the guards in §5.1 mean none of it is gating on `main` today.
