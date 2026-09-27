//! Cascade correctness at fleet scale, with and without failures.
//!
//! `scale_smoke.rs` already pins the happy path at 256 and 1024 nodes.
//! This file covers what it does not: the *degraded* path at the same
//! scales. A cascade that only converges when nothing goes wrong is not
//! evidence of anything on a real cluster.
//!
//! The specific claim under test is orphan re-rooting. When a node
//! dies mid-cascade, its descendants have no parent to fetch from. A
//! strategy that only walks the seeded tree leaves those nodes
//! stranded. `LevelTreeFanOut` walks *past* a failed parent to the
//! nearest living ancestor, so exactly one node — the dead one — fails
//! to converge, and the rest of the fleet still gets the closure.

use std::collections::HashSet;

use consortium_fanout_sim::fixtures::{BandwidthDistribution, FailureSchedule, UplinkDistribution};
use consortium_fanout_sim::link::{LinkDirection, LinkModel, PacketLoss};
use consortium_fanout_sim::{Scenario, ScenarioConfig};
use consortium_nix::cascade::{CascadeResult, NodeId};
use consortium_nix::cascade_strategies::LevelTreeFanOut;

const SCALES: [u32; 3] = [64, 256, 1024];

/// A mixed-speed fabric with contended uplinks, matching the shape
/// `scale_smoke` uses so results are comparable.
fn fleet(n: u32, seed: u64) -> ScenarioConfig {
    ScenarioConfig {
        seed,
        n_nodes: n,
        seed_fraction: 0.0,
        closure_bytes: 100 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 10 * 1024 * 1024,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: Some(UplinkDistribution::Bimodal {
            slow: 20 * 1024 * 1024,
            fast: 512 * 1024 * 1024,
            fast_fraction: 0.4,
        }),
        failures: FailureSchedule::None,
        max_rounds: 64,
    }
}

fn converged_ids(r: &CascadeResult) -> HashSet<NodeId> {
    r.converged.iter().copied().collect()
}

#[test]
fn a_dead_node_at_scale_strands_exactly_one_host() {
    // Orphan re-rooting: kill a mid-tree node and check the blast
    // radius is exactly itself. If re-rooting were broken, every
    // descendant of the dead node would also fail to converge.
    for n in SCALES {
        // Fanout 2 => a balanced binary tree, so the dead node has
        // descendants and re-rooting actually has to kick in.
        let strategy = LevelTreeFanOut::new(2);
        let victim = NodeId((n / 2) - 1);
        let mut cfg = fleet(n, 0xA11CE_0000 + n as u64);
        cfg.failures = FailureSchedule::KillNodeAtRound { node: victim, round: 0 };

        let r = Scenario::new(cfg).run(&strategy);
        let got = converged_ids(&r);
        assert_eq!(
            got.len(),
            n as usize - 1,
            "n={n}: converged {} of {n}, expected exactly one casualty",
            got.len()
        );
        assert!(
            !got.contains(&victim),
            "n={n}: dead node {victim} reported as converged"
        );
        assert!(!r.is_success(), "n={n}: a dead node should fail the run");
        // No host left behind: every node except the victim received
        // the closure, including the victim's own children.
        for i in 0..n {
            let id = NodeId(i);
            if id != victim {
                assert!(got.contains(&id), "n={n}: {id} was left behind");
            }
        }
    }
}

#[test]
fn several_dead_nodes_still_orphan_reroot_at_scale() {
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        // Kill a spread of interior nodes, not just one.
        let victims: Vec<NodeId> = (0..n)
            .step_by((n / 8) as usize)
            .map(|i| NodeId(i))
            .filter(|i| i.0 > 0 && *i != NodeId(0))
            .collect();
        assert!(victims.len() >= 3, "n={n}: not enough victims");

        // KillNodeAtRound only takes one node, so run them in sequence
        // and union the casualties: each run must strand only its own
        // victim.
        for v in &victims {
            let mut cfg = fleet(n, 0xB0B_0000 + n as u64);
            cfg.failures = FailureSchedule::KillNodeAtRound { node: *v, round: 0 };
            let r = Scenario::new(cfg).run(&strategy);
            let got = converged_ids(&r);
            assert_eq!(
                got.len(),
                n as usize - 1,
                "n={n} victim={v}: stranded {} nodes, expected 1",
                n as usize - 1 - got.len()
            );
            assert!(!got.contains(v), "n={n} victim={v} converged anyway");
        }
    }
}

#[test]
fn killing_the_seed_is_harmless_because_seeds_are_already_converged() {
    // Worth pinning because it looks like it should be the worst case
    // and is not. The seed is pre-loaded, so no cascade edge ever
    // *targets* it; `KillNodeAtRound{node: 0}` therefore matches no
    // edge and the deploy proceeds normally. The coordinator is not
    // reporting a false success — there was never work to do.
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        let mut cfg = fleet(n, 0xC0DE_0000 + n as u64);
        cfg.failures = FailureSchedule::KillNodeAtRound {
            node: NodeId(0),
            round: 0,
        };
        let r = Scenario::new(cfg).run(&strategy);
        assert!(
            r.is_success(),
            "n={n}: killing the pre-converged seed should not fail the run"
        );
        assert_eq!(r.converged.len(), n as usize);
    }
}

#[test]
fn a_fully_dead_fabric_converges_only_the_seed() {
    // The honest-failure case. With every edge failing, there is no
    // path to any non-seed host, and the harness must say so rather
    // than inventing convergence or hanging. The seed still counts:
    // it was seeded, not delivered to.
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        let mut cfg = fleet(n, 0xDEAD_0000 + n as u64);
        cfg.failures = FailureSchedule::Random {
            fraction: 1.0,
            seed: 0xDEAD_0000 + n as u64,
        };
        let r = Scenario::new(cfg).run(&strategy);
        assert!(!r.is_success(), "n={n}: total fabric loss reported success");
        assert_eq!(
            r.converged,
            vec![NodeId(0)],
            "n={n}: only the seed should be converged"
        );
        assert!(
            r.failed.is_some(),
            "n={n}: a failed run must carry an error tree"
        );
        if let Some(err) = &r.failed {
            let affected = err.affected_nodes();
            assert!(!affected.is_empty(), "n={n}: empty error tree");
            for id in affected {
                assert!(id.0 < n, "n={n}: error tree names unknown node {id}");
            }
        }
    }
}

#[test]
fn convergence_stays_complete_under_a_lossy_link_at_every_scale() {
    // Packet loss is a timing effect, not a partition: it must slow the
    // cascade without dropping a single host.
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        let cfg = fleet(n, 0x1055_0000 + n as u64);
        let lossy = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.05))
            .with_jitter(0.2)
            .with_direction(LinkDirection::UplinkLimited);
        let r = Scenario::new(cfg.clone()).run_with_link(&strategy, lossy);
        assert!(
            r.is_success(),
            "n={n}: lossy cascade failed: {:?}",
            r.failed.as_ref().map(|e| e.to_string())
        );
        assert_eq!(r.converged.len(), n as usize, "n={n}: hosts left behind");
    }
}

#[test]
fn a_lossy_fabric_converges_in_the_same_number_of_rounds() {
    // Loss buys time, not topology: round count is a property of the
    // strategy and the failure schedule, not of how slow the wire is.
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        let cfg = fleet(n, 0x2055_0000 + n as u64);
        let clean = Scenario::new(cfg.clone()).run(&strategy);
        let lossy = Scenario::new(cfg).run_with_link(
            &strategy,
            LinkModel::new()
                .with_loss(PacketLoss::PerChunk(0.05))
                .with_jitter(0.2),
        );
        assert_eq!(
            clean.rounds, lossy.rounds,
            "n={n}: loss changed the round count"
        );
        let clean_total: f64 = clean.round_durations.iter().map(|d| d.as_secs_f64()).sum();
        let lossy_total: f64 = lossy.round_durations.iter().map(|d| d.as_secs_f64()).sum();
        assert!(
            lossy_total > clean_total,
            "n={n}: lossy total {lossy_total:.3}s not above clean {clean_total:.3}s"
        );
    }
}

#[test]
fn failed_runs_stay_reproducible_at_scale() {
    // Determinism has to survive the degraded path, or the reports
    // that consume these numbers are noise.
    for n in SCALES {
        let strategy = LevelTreeFanOut::new(2);
        let mut cfg = fleet(n, 0x30FA_0000 + n as u64);
        cfg.failures = FailureSchedule::KillNodeAtRound {
            node: NodeId(n / 4),
            round: 0,
        };
        let a = Scenario::new(cfg.clone()).run(&strategy);
        let b = Scenario::new(cfg).run(&strategy);
        assert_eq!(a.rounds, b.rounds, "n={n}");
        assert_eq!(a.round_durations, b.round_durations, "n={n}");
        assert_eq!(converged_ids(&a), converged_ids(&b), "n={n}");
    }
}
