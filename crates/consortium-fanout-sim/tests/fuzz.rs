//! Property-based fuzz tests for cascade strategies.
//!
//! Generates random scenarios across the cross-product of
//! `(n_nodes, seed_fraction, bandwidth_dist, failures, strategy)` and
//! asserts universal invariants. Each generated case is reproducible
//! from its proptest seed.
//!
//! Run with `PROPTEST_CASES=N` to control case count (default 256).
//! CI should set `PROPTEST_CASES=32` for ~2 min runs; local
//! exploration can use 1024+.

use std::collections::HashSet;

use consortium_fanout_sim::{
    fixtures::{BandwidthDistribution, FailureSchedule},
    scenario::{Scenario, ScenarioConfig},
};
use consortium_nix::cascade::{CascadeStrategy, Log2FanOut, NodeId};
use consortium_nix::cascade_strategies::{MaxBottleneckSpanning, SteinerGreedy};
use proptest::prelude::*;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

fn pick_strategy(idx: u8) -> &'static dyn CascadeStrategy {
    match idx % 3 {
        0 => &Log2FanOut,
        1 => &MaxBottleneckSpanning,
        _ => &SteinerGreedy,
    }
}

fn bandwidth_strategy() -> impl Strategy<Value = BandwidthDistribution> {
    prop_oneof![
        // Uniform: bandwidth in [10 MB/s, 1 GB/s]
        (10u64 * 1024 * 1024..1024 * 1024 * 1024).prop_map(BandwidthDistribution::Uniform),
        // Bimodal: slow << fast, fast_fraction varies
        (
            1u64 * 1024 * 1024..50 * 1024 * 1024,
            100u64 * 1024 * 1024..2 * 1024 * 1024 * 1024,
            0.05f64..0.95,
        )
            .prop_map(
                |(slow, fast, fast_fraction)| BandwidthDistribution::Bimodal {
                    slow,
                    fast,
                    fast_fraction,
                }
            ),
    ]
}

proptest! {
    // Aggressive but reasonable bound — cases should run in <100ms
    // each since the sim is deterministic + in-process.
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 32,
        .. ProptestConfig::default()
    })]

    #[test]
    fn cascade_universal_invariants_hold(
        seed in 0u64..u64::MAX,
        n_nodes in 8u32..=128,
        seed_fraction in 0.0f64..=0.5,
        closure_mb in 1u64..=200,
        bandwidth in bandwidth_strategy(),
        strategy_idx in 0u8..=2,
    ) {
        let strategy = pick_strategy(strategy_idx);
        let cfg = ScenarioConfig {
            seed,
            n_nodes,
            seed_fraction,
            closure_bytes: closure_mb * 1024 * 1024,
            bandwidth,
            uplinks: None,
            failures: FailureSchedule::None,
            max_rounds: 64,
        };
        let result = Scenario::new(cfg.clone()).run(strategy);

        // Universal invariants under no-failure scenarios.
        prop_assert!(
            result.is_success(),
            "[{}] failed unexpectedly: {:?}",
            strategy.name(),
            result.failed,
        );
        prop_assert_eq!(
            result.converged.len() as u32,
            cfg.n_nodes,
            "[{}] not all nodes converged",
            strategy.name(),
        );
        let converged_set: HashSet<NodeId> =
            result.converged.iter().copied().collect();
        prop_assert_eq!(
            converged_set.len(),
            result.converged.len(),
            "[{}] duplicates in converged list",
            strategy.name(),
        );
        prop_assert!(
            // Loose bound: rounds capped by coordinator's max_rounds.
            // Was previously `rounds <= n_nodes` but retry-on-failure
            // semantics (since the coordinator stopped marking targets
            // permanently failed on single edge failures) means an
            // unlucky failure pattern can legitimately exceed n_nodes
            // rounds. The strategy keeps trying alternate sources.
            result.rounds <= cfg.max_rounds,
            "[{}] rounds {} > max_rounds {}",
            strategy.name(),
            result.rounds,
            cfg.max_rounds,
        );
        prop_assert_eq!(
            result.round_durations.len() as u32,
            result.rounds,
            "[{}] round_durations len mismatch",
            strategy.name(),
        );
    }

    #[test]
    fn cascade_invariants_with_failures(
        seed in 0u64..u64::MAX,
        n_nodes in 16u32..=64,
        bandwidth in bandwidth_strategy(),
        strategy_idx in 0u8..=2,
        failure_seed in 0u64..u64::MAX,
    ) {
        let strategy = pick_strategy(strategy_idx);
        let mut frng = ChaCha8Rng::seed_from_u64(failure_seed);
        // Sample a failure deterministically from this case's seed —
        // proptest's strategy combinators don't compose with our n_nodes
        // dependency cleanly, so we sample manually.
        let failure_kind: u8 = frng.gen_range(0u8..=2);
        let killed_node: Option<NodeId> = None;
        let killed_node = match failure_kind {
            1 => Some(NodeId(frng.gen_range(0..n_nodes))),
            _ => killed_node,
        };
        let failures = match failure_kind {
            0 => FailureSchedule::None,
            1 => FailureSchedule::KillNodeAtRound {
                node: killed_node.unwrap(),
                round: frng.gen_range(0..6),
            },
            _ => {
                let s = frng.gen_range(0..n_nodes);
                let mut t = frng.gen_range(0..n_nodes);
                if t == s {
                    t = (t + 1) % n_nodes;
                }
                FailureSchedule::PartitionAtRound {
                    src: NodeId(s),
                    tgt: NodeId(t),
                    round: frng.gen_range(0..6),
                }
            }
        };

        let cfg = ScenarioConfig {
            seed,
            n_nodes,
            seed_fraction: 0.0,
            closure_bytes: 10 * 1024 * 1024,
            bandwidth,
            uplinks: None,
            failures,
            max_rounds: 64,
        };
        let result = Scenario::new(cfg.clone()).run(strategy);

        // Even with failures, sanity bounds must hold.
        let converged_set: HashSet<NodeId> =
            result.converged.iter().copied().collect();
        prop_assert_eq!(
            converged_set.len(),
            result.converged.len(),
            "[{}] duplicates in converged list",
            strategy.name(),
        );
        prop_assert!(
            // Loose bound: rounds capped by coordinator's max_rounds.
            // Was previously `rounds <= n_nodes` but retry-on-failure
            // semantics (since the coordinator stopped marking targets
            // permanently failed on single edge failures) means an
            // unlucky failure pattern can legitimately exceed n_nodes
            // rounds. The strategy keeps trying alternate sources.
            result.rounds <= cfg.max_rounds,
            "[{}] rounds {} > max_rounds {}",
            strategy.name(),
            result.rounds,
            cfg.max_rounds,
        );
        prop_assert_eq!(
            result.round_durations.len() as u32,
            result.rounds,
            "[{}] round_durations len mismatch",
            strategy.name(),
        );
        // If failed Some, every affected node id must be valid.
        if let Some(err) = &result.failed {
            for nid in err.affected_nodes() {
                prop_assert!(
                    nid.0 < cfg.n_nodes,
                    "[{}] error references invalid node {}",
                    strategy.name(),
                    nid
                );
            }
        }

        // When KillNodeAtRound is injected at round 0, the killed node
        // MUST appear in the failure tree — it can never receive the
        // closure because every copy attempt to it fails from round 0.
        //
        // One precondition the older version of this assertion missed:
        // this case runs with `seed_fraction: 0.0`, which makes
        // `Scenario::run` use `SeedDistribution::Single` — and that
        // always seeds `NodeId(0)` (fixtures.rs). A kill targeting the
        // seed node is a *no-op*: the seed already holds the closure and
        // no edge ever targets it, so the schedule never fires, the
        // cascade converges fully, and `result.failed` is `None`. The
        // assertion below then failed on a correct simulator.
        //
        // Confirmed against CI: `seed=4218163067, n_nodes=21,
        // strategy_idx=1, failure_seed=15529019868851313450` samples
        // `killed = NodeId(0)`, and proptest's random generator hits that
        // combination often enough to redden `master` and five stacked
        // PRs. So gate on "the kill can actually fire" rather than
        // assuming it can.
        if let Some(killed) = killed_node {
            // The seed set is `SeedDistribution::Single` = {NodeId(0)}
            // whenever seed_fraction == 0.0, independent of `seed`.
            let is_seed_node = killed == NodeId(0);
            // Round 0 means it fires on the first attempt regardless of
            // strategy. Round > 0 may not fire if the cascade halts
            // before then (valid for Steiner on uniform).
            let fires_at_round_zero = matches!(
                cfg.failures,
                FailureSchedule::KillNodeAtRound { round: 0, .. }
            );
            if fires_at_round_zero && !is_seed_node {
                prop_assert!(
                    !result.converged.iter().any(|&n| n == killed),
                    "[{}] killed node {killed:?} still appears in converged set: {:?}",
                    strategy.name(),
                    result.converged,
                );
                let err = result.failed.as_ref().unwrap_or_else(|| {
                    panic!(
                        "[{}] killed node injected at round 0 but result.failed is None",
                        strategy.name()
                    )
                });
                prop_assert!(
                    err.affected_nodes().contains(&killed),
                    "[{}] killed node {killed:?} missing from affected set",
                    strategy.name(),
                );
            } else if is_seed_node {
                // The kill is a no-op, so the cascade must converge
                // *cleanly* — no failure tree at all. Asserting this
                // keeps the skipped branch honest instead of silent.
                prop_assert!(
                    result.is_success(),
                    "[{}] killing the seed node should be a no-op, but the \
                     cascade reported a failure: {:?}",
                    strategy.name(),
                    result.failed,
                );
                prop_assert!(
                    result.converged.len() as u32 == cfg.n_nodes,
                    "[{}] killing the seed node should not stop convergence; \
                     got {}/{} converged",
                    strategy.name(),
                    result.converged.len(),
                    cfg.n_nodes,
                );
            }
        }
    }

    #[test]
    fn scenario_is_deterministic_in_seed(
        seed in 0u64..u64::MAX,
        n_nodes in 8u32..=64,
    ) {
        let cfg = ScenarioConfig {
            seed,
            n_nodes,
            seed_fraction: 0.0,
            closure_bytes: 10 * 1024 * 1024,
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 5 * 1024 * 1024,
                fast: 500 * 1024 * 1024,
                fast_fraction: 0.4,
            },
            uplinks: None,
            failures: FailureSchedule::None,
            max_rounds: 32,
        };
        let r1 = Scenario::new(cfg.clone()).run(&MaxBottleneckSpanning);
        let r2 = Scenario::new(cfg).run(&MaxBottleneckSpanning);
        prop_assert_eq!(r1.rounds, r2.rounds);
        prop_assert_eq!(r1.round_durations.clone(), r2.round_durations.clone());
        // Tightened: full set equality, not just len(). Catches the case
        // where determinism produces the same COUNT of converged nodes
        // but a different SET — which would mean the cascade is making
        // non-deterministic edge choices we'd never notice with `len ==`.
        let s1: HashSet<NodeId> = r1.converged.iter().copied().collect();
        let s2: HashSet<NodeId> = r2.converged.iter().copied().collect();
        prop_assert_eq!(s1, s2, "converged sets diverge between identical-seed runs");
    }
}

/// Regression pin for the master-red CON-240 case. The proptest above
/// hit `killed = NodeId(0)` — which is always the seed node — and its
/// round-0 assertion then required a failure that can never occur. This
/// deterministic test replays the exact CI input and locks in the
/// corrected expectation: killing the seed is a no-op.
#[test]
fn killing_seed_node_is_a_noop() {
    // Exact input from the GitHub Actions failure at run 36308112275:
    //   minimal failing input: seed = 4218163067, n_nodes = 21,
    //   bandwidth = Bimodal { slow: 34207121, fast: 2127617055,
    //     fast_fraction: 0.22865491590967524 },
    //   strategy_idx = 1, failure_seed = 15529019868851313450
    let mut frng = ChaCha8Rng::seed_from_u64(15529019868851313450);
    let failure_kind: u8 = frng.gen_range(0u8..=2);
    assert_eq!(failure_kind, 1, "precondition: failure_kind must be kill");
    let killed = NodeId(frng.gen_range(0..21));
    assert_eq!(killed, NodeId(0), "precondition: killed node must be the seed");
    let round = frng.gen_range(0..6);
    assert_eq!(round, 0, "precondition: kill must be injected at round 0");

    let cfg = ScenarioConfig {
        seed: 4218163067,
        n_nodes: 21,
        seed_fraction: 0.0,
        closure_bytes: 10 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 34207121,
            fast: 2127617055,
            fast_fraction: 0.22865491590967524,
        },
        uplinks: None,
        failures: FailureSchedule::KillNodeAtRound {
            node: killed,
            round,
        },
        max_rounds: 64,
    };
    let result = Scenario::new(cfg.clone()).run(&MaxBottleneckSpanning);
    assert!(
        result.is_success(),
        "killing the seed node must be a no-op; got {:?}",
        result.failed
    );
    assert_eq!(result.converged.len() as u32, cfg.n_nodes);
}

/// Regression pin for the real invariant the fuzz assertion protects:
/// a round-0 kill of a NON-seed node must converge every other node,
/// report the failure, and reference the killed node in affected_nodes.
#[test]
fn killing_nonseed_node_at_round0_reports_failure() {
    let n_nodes = 21;
    let killed = NodeId(9); // non-seed
    let cfg = ScenarioConfig {
        seed: 4218163067,
        n_nodes,
        seed_fraction: 0.0,
        closure_bytes: 10 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 34207121,
            fast: 2127617055,
            fast_fraction: 0.22865491590967524,
        },
        uplinks: None,
        failures: FailureSchedule::KillNodeAtRound {
            node: killed,
            round: 0,
        },
        max_rounds: 64,
    };
    for strategy in [
        &Log2FanOut as &dyn CascadeStrategy,
        &MaxBottleneckSpanning,
        &SteinerGreedy,
    ] {
        let result = Scenario::new(cfg.clone()).run(strategy);
        assert!(
            !result.converged.iter().any(|&n| n == killed),
            "[{}] killed node {killed:?} still in converged: {:?}",
            strategy.name(),
            result.converged,
        );
        let err = result.failed.as_ref().expect("expected a failure tree");
        assert!(
            err.affected_nodes().contains(&killed),
            "[{}] killed node {killed:?} missing from affected: {:?}",
            strategy.name(),
            err.affected_nodes(),
        );
        // Everything except the killed node converges.
        assert_eq!(result.converged.len() as u32, cfg.n_nodes - 1);
    }
}

