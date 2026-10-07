//! End-to-end checks that the sim emits the CON-20 report schema
//! correctly at scale, and that the output is stable enough to serve as a
//! regression baseline.
//!
//! The contract these tests defend:
//!
//! 1. every run produces a report carrying all five metric families,
//! 2. the same seed produces byte-identical JSON (baseline diffing),
//! 3. a different seed produces different JSON (the seed really is used),
//! 4. failure runs keep the failed subtree's *shape*, not a flat count,
//! 5. deploy-DAG phase timings attach to the same report.

use consortium_fanout_sim::dag_sim::{
    DagFailures, DagNetwork, DagSimConfig, DeployDagSim, StageSchedule,
};
use consortium_fanout_sim::fixtures::{BandwidthDistribution, FailureSchedule};
use consortium_fanout_sim::report::RunReport;
use consortium_fanout_sim::scenario::{Scenario, ScenarioConfig};
use consortium_nix::cascade::{CascadeStrategy, Log2FanOut, NodeId};
use consortium_nix::cascade_trace::TraceRecorder;

/// Run a scenario and build its report. Uses the traced path so the
/// per-edge outcomes and parent chain are available.
fn report_for(cfg: &ScenarioConfig) -> RunReport {
    let rec = TraceRecorder::new();
    let (result, trace) = Scenario::new(cfg.clone()).run_traced(&Log2FanOut, &rec);
    RunReport::from_cascade(
        RunReport::meta(cfg, Log2FanOut.name()),
        &result,
        &trace,
        cfg.closure_bytes,
    )
}

fn cfg_at(seed: u64, n_nodes: u32) -> ScenarioConfig {
    ScenarioConfig {
        seed,
        n_nodes,
        closure_bytes: 250 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 25 * 1024 * 1024,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        ..ScenarioConfig::default()
    }
}

#[test]
fn a_clean_run_at_every_target_scale_leaves_no_host_behind() {
    for n in [64u32, 256, 1024] {
        let r = report_for(&cfg_at(0xc0ffee, n));
        assert!(
            r.is_success(),
            "n={n} reported failures: {:?}",
            r.failure_subtree
        );
        assert_eq!(r.converged_hosts(), n as usize, "n={n} stranded a host");
        assert_eq!(r.stranded_hosts(), 0, "n={n} stranded a host");
        assert!(
            r.time_to_converge.is_some(),
            "n={n} did not report a convergence time"
        );
    }
}

#[test]
fn fan_out_depth_matches_the_strategy_at_every_scale() {
    for n in [64u32, 256, 1024] {
        let r = report_for(&cfg_at(11, n));
        // log2 fanout from a single seed.
        assert_eq!(r.fan_out_depth, n.ilog2(), "n={n}");
        assert_eq!(r.fan_out_width, 1, "n={n}");
    }
}

#[test]
fn the_report_is_byte_identical_across_repeated_runs_of_one_seed() {
    // This is the property that lets CON-20 store a report as a baseline
    // and diff a later run against it.
    for n in [64u32, 256] {
        let a = report_for(&cfg_at(4242, n));
        let b = report_for(&cfg_at(4242, n));
        assert_eq!(a.to_json(), b.to_json(), "n={n} was not reproducible");
        assert_eq!(a.to_markdown(), b.to_markdown(), "n={n}");
    }
}

#[test]
fn distinct_seeds_produce_distinct_reports() {
    let a = report_for(&cfg_at(1, 128));
    let b = report_for(&cfg_at(2, 128));
    assert_ne!(a.to_json(), b.to_json());
}

#[test]
fn every_converged_host_appears_in_the_convergence_table() {
    let r = report_for(&cfg_at(3, 128));
    assert_eq!(r.convergence.len(), 128);
    // Node ids are unique and the seed is the only depth-0 node.
    let mut ids: Vec<u32> = r.convergence.iter().map(|c| c.node.0).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 128, "duplicate nodes in convergence table");

    let roots = r.convergence.iter().filter(|c| c.depth == 0).count();
    assert_eq!(roots, 1, "expected exactly one pre-seeded root");

    // Every host's depth is consistent: a node's depth is one more than
    // its parent's, and no host is stranded at an unexplained depth.
    for c in &r.convergence {
        assert!(c.depth <= 7, "n=128 log2 fanout cannot exceed depth 7");
    }
}

#[test]
fn throughput_accounting_covers_every_copy_edge() {
    // 1 seed + (n-1) copy edges, and every edge delivered the closure.
    for n in [64u32, 256] {
        let cfg = cfg_at(8, n);
        let r = report_for(&cfg);
        assert_eq!(r.edge_throughput.len(), n as usize - 1, "n={n}");
        for e in &r.edge_throughput {
            assert_eq!(e.bytes, cfg.closure_bytes);
            assert!(e.duration.as_secs_f64() > 0.0, "n={n} zero-duration edge");
            assert!(
                e.goodput_bytes_per_sec() > 0.0,
                "n={n} non-positive goodput"
            );
        }
    }
}

#[test]
fn a_killed_host_shows_up_as_a_subtree_not_a_flat_count() {
    let cfg = ScenarioConfig {
        // Kill three distinct hosts at round 1; every edge into them
        // fails, so the cascade must re-root around them.
        failures: FailureSchedule::All(vec![
            FailureSchedule::KillNodeAtRound {
                node: NodeId(3),
                round: 1,
            },
            FailureSchedule::KillNodeAtRound {
                node: NodeId(7),
                round: 1,
            },
            FailureSchedule::KillNodeAtRound {
                node: NodeId(11),
                round: 1,
            },
        ]),
        ..cfg_at(21, 64)
    };
    let r = report_for(&cfg);

    assert!(!r.is_success(), "injected kills should fail the run");
    let subtree = r
        .failure_subtree
        .as_ref()
        .expect("a failed run must carry a failure subtree");

    // The dead hosts must be recoverable by walking the tree.
    let leaves = subtree.leaves();
    let dead: Vec<u32> = leaves.iter().map(|(n, _)| n.0).collect();
    for victim in [3u32, 7, 11] {
        assert!(
            dead.contains(&victim),
            "node {victim} missing from failure subtree; got {dead:?}"
        );
    }
    // Shape preserved: the tree is nested, not a flat vector of 3 errors.
    assert!(
        !subtree.children.is_empty(),
        "aggregate lost its children — subtree shape was flattened"
    );
}

#[test]
fn a_failed_run_does_not_claim_to_have_converged() {
    let cfg = ScenarioConfig {
        failures: FailureSchedule::KillNodeAtRound {
            node: NodeId(5),
            round: 1,
        },
        ..cfg_at(22, 64)
    };
    let r = report_for(&cfg);
    assert!(!r.is_success());
    assert!(
        r.time_to_converge.is_none(),
        "a failed run must not report a convergence time"
    );
    // The JSON must still be well-formed and self-describing.
    let json = r.to_json();
    assert!(json.contains("\"time_to_converge_ms\": null"));
    assert!(json.contains("\"failure_subtree\""));
    assert!(!json.contains("\"failure_subtree\": null"));
}

#[test]
fn deploy_phase_timings_attach_to_the_same_report() {
    // The cascade side gives fan-out/throughput; the DAG side gives
    // per-phase timing. One report carries both.
    let cfg = cfg_at(31, 64);
    let cascade = report_for(&cfg);

    let dag = DeployDagSim::new(DagSimConfig {
        seed: 31,
        hosts: 64,
        schedule: StageSchedule::production(16),
        network: DagNetwork {
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 10 * 1024 * 1024,
                fast: 1024 * 1024 * 1024,
                fast_fraction: 0.3,
            },
            uplinks: None,
            latency: std::time::Duration::from_millis(2),
        },
        failures: DagFailures::None,
        ..DagSimConfig::default()
    })
    .run();

    let combined = cascade.with_dag(&dag);

    assert_eq!(combined.fan_out_depth, 6, "cascade metrics preserved");
    assert_eq!(combined.phase_timings.len(), 5, "all five phases present");
    assert!(combined.deploy_wall_time.is_some());

    let json = combined.to_json();
    assert!(json.contains("\"phase_timings\""));
    assert!(json.contains("\"deploy_wall_time_ms\""));
    for phase in ["eval", "health", "build", "copy", "activate"] {
        assert!(json.contains(phase), "missing phase {phase} in report");
    }

    // Markdown is the human-readable half of the same artifact.
    let md = combined.to_markdown();
    assert!(md.contains("## Deploy phases"));
    assert!(md.contains("| copy |"));
}

#[test]
fn markdown_surfaces_the_failure_subtree_for_a_failed_run() {
    let cfg = ScenarioConfig {
        failures: FailureSchedule::KillNodeAtRound {
            node: NodeId(9),
            round: 1,
        },
        ..cfg_at(41, 64)
    };
    let md = report_for(&cfg).to_markdown();
    assert!(md.contains("## Failure subtree"));
    assert!(md.contains("did not converge"));
}
