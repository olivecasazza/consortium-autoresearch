//! The reference scenarios from the testing-requirements rubric
//! (`docs/testing-requirements.md` §5), implemented as tests.
//!
//! The rubric defines four baseline-lock scenarios and is explicit that
//! most thresholds start as `[target]` — "a number chosen for the
//! framework to hit; **not** evidence of anything today". This file is
//! the evidence. Each scenario is built at the exact parameters the
//! rubric specifies, run, and the outcome pinned.
//!
//! Thresholds the rubric marks `[measured]`/`[derived]` are asserted as
//! written. Thresholds it marks `[target]` are calibrated here against a
//! real run and then frozen, which is what the rubric asks for ("90 %
//! re-rooting is a judgement call at the moment — it should be calibrated
//! against the first real run, then frozen"). Each calibrated value is
//! recorded beside its assertion.
//!
//! Two results contradict the rubric as written. Both are asserted here
//! as the true behaviour, with the contradiction called out, because
//! silently "fixing" either would hide a finding the rubric needs:
//!
//! - **S2's "per-node credit" threshold is unreachable in the cascade.**
//!   `CascadePlan` requires each source at most once per round, so no
//!   node ever serves two targets in the same round and uplink
//!   contention cannot arise. Contention is real, but it belongs to the
//!   deploy DAG's parallel `copy`, not to the cascade.
//! - **S2's "30/70 bimodal edge bandwidth" is inert** once a uniform
//!   6 250 000 B/s uplink is set, because `min(edge_bw, uplink)` pins
//!   every edge to the uplink. All 256 edges come out at one duration.
//!
//! Seeds are fixed and every scenario asserts reproducibility.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use consortium_fanout_sim::fixtures::{BandwidthDistribution, FailureSchedule, UplinkDistribution};
use consortium_fanout_sim::link::{LinkModel, PacketLoss};
use consortium_fanout_sim::report::RunReport;
use consortium_fanout_sim::scenario::{Scenario, ScenarioConfig};
use consortium_nix::cascade::{CascadeError, CascadeResult, CascadeStrategy, NodeId};
use consortium_nix::cascade_strategies::{LevelTreeFanOut, MaxBottleneckSpanning};
use consortium_nix::cascade_trace::TraceRecorder;

/// 50 Mbit/s expressed in bytes/sec — the rubric's unit convention.
const FIFTY_MBIT: u64 = 6_250_000;

/// The rubric's modelled 10 MiB closure for S2/S3.
const CLOSURE_10MIB: u64 = 10 * 1024 * 1024;

/// The S3 partition fires at this round ("mid-flight" per the rubric).
const PARTITION_ROUND: u32 = 4;

fn converged_ids(r: &CascadeResult) -> HashSet<NodeId> {
    r.converged.iter().copied().collect()
}

// ============================================================================
// S2 — Asymmetric-link cascade at fabric rates
// ============================================================================

/// S2: N=256, two zones, per-node uplink 6 250 000 B/s, downlink
/// 4x uplink, 30/70 bimodal edge bandwidth, `MaxBottleneckSpanning`.
fn s2_config() -> ScenarioConfig {
    ScenarioConfig {
        seed: 0x00A2,
        n_nodes: 256,
        seed_fraction: 0.0,
        closure_bytes: CLOSURE_10MIB,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 6_250_000,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: Some(UplinkDistribution::Uniform(FIFTY_MBIT)),
        failures: FailureSchedule::None,
        max_rounds: 64,
    }
}

#[test]
fn s2_every_node_converges_on_an_asymmetric_fabric() {
    let cfg = s2_config();
    let r = Scenario::new(cfg).run(&MaxBottleneckSpanning);
    assert!(
        r.is_success(),
        "S2 must converge with no failures: {:?}",
        r.failed
    );
    // Rubric: "Converged == 256" [derived].
    assert_eq!(r.converged.len(), 256);
}

#[test]
fn s2_fan_out_depth_lands_on_the_log2_bound() {
    let cfg = s2_config();
    let rec = TraceRecorder::new();
    let (r, trace) = Scenario::new(cfg.clone()).run_traced(&MaxBottleneckSpanning, &rec);
    let report = RunReport::from_cascade(
        RunReport::meta(&cfg, MaxBottleneckSpanning.name()),
        &r,
        &trace,
        cfg.closure_bytes,
    );
    // Rubric: "Fan-out depth <= ceil(log2 256) == 8" [derived].
    assert!(
        report.fan_out_depth <= 8,
        "depth {} exceeded the log2 bound of 8",
        report.fan_out_depth
    );
    // Calibrated: 8 — the strategy hits the bound exactly.
    assert_eq!(report.fan_out_depth, 8, "measured fan-out depth changed");
}

#[test]
fn s2_time_to_converge_meets_the_rubrics_closed_form() {
    let cfg = s2_config();
    let rec = TraceRecorder::new();
    let (r, trace) = Scenario::new(cfg.clone()).run_traced(&MaxBottleneckSpanning, &rec);
    let report = RunReport::from_cascade(
        RunReport::meta(&cfg, MaxBottleneckSpanning.name()),
        &r,
        &trace,
        cfg.closure_bytes,
    );

    let modelled = report
        .time_to_converge
        .expect("a successful run reports a convergence time")
        .as_secs_f64();

    // Rubric [derived]: 8 rounds x (10 MiB / 6.25 MB/s + latency)
    // ~= 13.5 s. Calibrated: 13.45 s, so the closed form holds as
    // written — no contention penalty, for the structural reason
    // asserted in `s2_uplink_contention_is_structurally_unreachable`.
    const RUBRIC_BOUND_S: f64 = 13.5;
    assert!(
        modelled <= RUBRIC_BOUND_S,
        "S2 converged in {modelled:.2}s, over the rubric's {RUBRIC_BOUND_S}s bound"
    );
    // Calibrated floor: 8 rounds of 10 MiB at 6.25 MB/s cannot be faster.
    const FLOOR_S: f64 = 13.0;
    assert!(
        modelled >= FLOOR_S,
        "S2 converged in {modelled:.2}s, implausibly under the {FLOOR_S}s floor"
    );
}

#[test]
fn s2_uplink_contention_is_structurally_unreachable() {
    // Rubric S2 asks for "per-node credit: no node credited more than its
    // uplink; uplink-bound round duration is the max over its out-edges".
    //
    // That threshold cannot be met *or* broken by the cascade, because
    // `CascadePlan` requires each source to appear at most once per
    // round: a node never has more than one out-edge in a round, so
    // `uplink / K` is always `uplink / 1`. This asserts the structural
    // fact so the rubric can move the threshold to the deploy DAG,
    // where parallel `copy` really does divide an uplink K ways.
    let cfg = s2_config();
    let rec = TraceRecorder::new();
    let (_r, trace) = Scenario::new(cfg).run_traced(&MaxBottleneckSpanning, &rec);

    let mut worst = 0usize;
    for snap in &trace.snapshots {
        let mut by_src: HashMap<u32, usize> = HashMap::new();
        for ((src, _tgt), outcome) in &snap.outcomes {
            if outcome.is_ok() {
                *by_src.entry(src.0).or_default() += 1;
            }
        }
        for count in by_src.values() {
            worst = worst.max(*count);
        }
    }
    assert_eq!(
        worst, 1,
        "a source served {worst} targets in one round — the cascade plan \
         invariant (one target per source per round) has been relaxed, \
         which changes the S2 contention model"
    );
}

#[test]
fn s2_bimodal_edge_bandwidth_is_inert_under_a_uniform_uplink() {
    // The rubric specifies "30/70 bimodal edge bandwidth" *and* a uniform
    // 6 250 000 B/s uplink. The second masks the first: effective
    // bandwidth is `min(edge_bw, uplink)`, and no edge bandwidth in the
    // bimodal distribution exceeds the uplink, so every edge collapses to
    // the uplink and all durations are identical.
    //
    // Asserted so the rubric author can either drop the inert parameter
    // or raise the fast tier above the uplink if the two-zone
    // heterogeneity is meant to be observable.
    let cfg = s2_config();
    let rec = TraceRecorder::new();
    let (_r, trace) = Scenario::new(cfg).run_traced(&MaxBottleneckSpanning, &rec);

    let mut durations: Vec<Duration> = Vec::new();
    for snap in &trace.snapshots {
        for outcome in snap.outcomes.values() {
            if let Ok(d) = outcome {
                durations.push(*d);
            }
        }
    }
    assert!(!durations.is_empty(), "S2 produced no successful edges");

    let min = durations.iter().min().unwrap();
    let max = durations.iter().max().unwrap();
    assert_eq!(
        min, max,
        "edge durations now vary ({min:?}..={max:?}); the bimodal edge \
         bandwidth has become observable, so S2's parameters are no \
         longer inert"
    );
}

// ============================================================================
// S3 — Packet loss + mid-flight partition
// ============================================================================

/// S3: N=256, 5 % per-chunk loss, one subtree partitioned mid-flight at
/// round 4. `LevelTreeFanOut` is the strategy because it walks past a
/// failed parent to the nearest living ancestor, which is the mechanism
/// orphan re-rooting depends on.
fn s3_config(failures: FailureSchedule) -> ScenarioConfig {
    ScenarioConfig {
        seed: 0x00A3,
        n_nodes: 256,
        seed_fraction: 0.0,
        closure_bytes: CLOSURE_10MIB,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 6_250_000,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: Some(UplinkDistribution::Uniform(FIFTY_MBIT)),
        failures,
        max_rounds: 64,
    }
}

/// 5 % per-chunk loss, as the rubric specifies.
fn lossy_link() -> LinkModel {
    LinkModel::new().with_loss(PacketLoss::PerChunk(0.05))
}

/// The edge the partition targets.
///
/// `FailureSchedule` only fails edges the strategy actually *plans*, so
/// the partitioned `(src, tgt)` has to be a real round-4 edge. It is
/// discovered from a clean traced run rather than hard-coded — a guessed
/// edge silently never fires, which looks exactly like a passing test.
/// (The obvious guess, `(0, 3)`, is never planned: node 3's parent is
/// node 1.)
fn round_four_edge() -> (NodeId, NodeId) {
    let rec = TraceRecorder::new();
    let (_r, trace) =
        Scenario::new(s3_config(FailureSchedule::None)).run_traced(&LevelTreeFanOut::new(2), &rec);
    trace
        .snapshots
        .iter()
        .filter(|s| s.round == PARTITION_ROUND)
        .flat_map(|s| s.outcomes.keys().copied())
        .next()
        .expect("a 256-node fan-out plans edges at round 4")
}

fn partitioned_config() -> (ScenarioConfig, NodeId, NodeId) {
    let (src, tgt) = round_four_edge();
    let cfg = s3_config(FailureSchedule::PartitionAtRound {
        src,
        tgt,
        round: PARTITION_ROUND,
    });
    (cfg, src, tgt)
}

#[test]
fn s3_a_lossy_fabric_with_a_mid_flight_partition_terminates_within_the_round_bound() {
    let (cfg, _src, _tgt) = partitioned_config();
    let r = Scenario::new(cfg).run_with_link(&LevelTreeFanOut::new(2), lossy_link());
    // Rubric: "cascade terminates within <= ceil(log2 256) + 4 == 12 rounds".
    // Calibrated: 8 — the partition costs no extra rounds, because the
    // orphan re-roots within the same round budget.
    assert!(
        r.rounds <= 12,
        "S3 took {} rounds, over the bound of 12",
        r.rounds
    );
    assert_eq!(r.rounds, 8, "calibrated round count changed");
}

#[test]
fn s3_orphans_re_root_above_the_ninety_percent_floor() {
    let (cfg, _src, tgt) = partitioned_config();
    let r = Scenario::new(cfg.clone()).run_with_link(&LevelTreeFanOut::new(2), lossy_link());

    let got = converged_ids(&r);
    let stranded: Vec<u32> = (0..cfg.n_nodes)
        .map(NodeId)
        .filter(|n| !got.contains(n))
        .map(|n| n.0)
        .collect();

    let re_rooted = got.len() as f64 / cfg.n_nodes as f64;
    assert!(
        re_rooted >= 0.90,
        "only {:.1}% re-rooted (stranded: {stranded:?})",
        re_rooted * 100.0
    );
    // Calibrated — this is the number the rubric should freeze in place
    // of "a judgement call at the moment": 255 of 256 nodes recover via
    // a surviving path, and the single casualty is the partitioned node
    // itself. 5 % packet loss costs time, not membership.
    assert_eq!(
        stranded,
        vec![tgt.0],
        "expected exactly the partitioned node to be stranded"
    );
    assert_eq!(got.len(), 255, "calibrated re-rooted count changed");
}

#[test]
fn s3_never_reports_a_partition_as_success() {
    // The safety property the rubric calls non-negotiable.
    let (cfg, _src, _tgt) = partitioned_config();
    let r = Scenario::new(cfg).run_with_link(&LevelTreeFanOut::new(2), lossy_link());
    assert!(
        !r.is_success(),
        "a run with a partitioned edge must not report success"
    );
    // And no *other* node may be claimed converged in error: the 255 that
    // did converge did so over a live path, and the one that did not is
    // reported absent rather than silently included.
    assert_eq!(r.converged.len(), 255);
}

#[test]
fn s3_the_error_tree_names_the_partitioned_edge() {
    // Rubric S3 asks for "error walk yields leaves in depth order" with
    // parent-path bubbling preserved.
    //
    // Note the calibration: a *single* partition produces a single leaf
    // error, not a nested `SubtreeAggregate` — bubbling only appears
    // when a parent has more than one failed child, which
    // `correctness.rs` covers for `KillNodeAtRound`. So this asserts the
    // leaf and its shape; the nesting case is asserted elsewhere.
    let (cfg, src, tgt) = partitioned_config();
    let r = Scenario::new(cfg).run_with_link(&LevelTreeFanOut::new(2), lossy_link());

    let err = r
        .failed
        .as_ref()
        .expect("a partitioned run must carry an error tree");

    match err {
        CascadeError::Partitioned { src: s, tgt: t } => {
            assert_eq!((*s, *t), (src, tgt), "error names the wrong edge");
        }
        other => panic!("expected a Partitioned error, got {other:?}"),
    }

    // The report layer must preserve this rather than flattening it.
    let rec = TraceRecorder::new();
    let (r2, _t) = Scenario::new(s3_config(FailureSchedule::PartitionAtRound {
        src,
        tgt,
        round: PARTITION_ROUND,
    }))
    .run_with_link_traced(&LevelTreeFanOut::new(2), lossy_link(), &rec);
    let report = RunReport::from_cascade(
        RunReport::meta(
            &s3_config(FailureSchedule::PartitionAtRound {
                src,
                tgt,
                round: PARTITION_ROUND,
            }),
            "level-tree-fanout-2",
        ),
        &r2,
        &_t,
        CLOSURE_10MIB,
    );
    let subtree = report
        .failure_subtree
        .as_ref()
        .expect("the report must carry the failure subtree");
    assert_eq!(subtree.kind, "Partitioned");
    assert_eq!(subtree.node, tgt);
    assert!(!subtree.transient, "a partition is not retryable in place");
    assert_eq!(subtree.leaves(), vec![(tgt, "Partitioned")]);
}

#[test]
fn s3_is_byte_for_byte_reproducible() {
    let (cfg, _src, _tgt) = partitioned_config();
    let run = || {
        let rec = TraceRecorder::new();
        let (r, _t) = Scenario::new(cfg.clone()).run_with_link_traced(
            &LevelTreeFanOut::new(2),
            lossy_link(),
            &rec,
        );
        let mut ids: Vec<u32> = r.converged.iter().map(|n| n.0).collect();
        ids.sort_unstable();
        let report = RunReport::from_cascade(
            RunReport::meta(&cfg, "level-tree-fanout-2"),
            &r,
            &_t,
            CLOSURE_10MIB,
        );
        (ids, format!("{:?}", r.failed), r.rounds, report.to_json())
    };
    assert_eq!(run(), run(), "S3 was not reproducible at a fixed seed");
}
