//! Reproducible generators for cascade scenarios.
//!
//! All generators take an `&mut StdRng` (or `ChaCha8Rng`) seeded from
//! the scenario's master seed, so a given `(seed, descriptor)` pair
//! produces deterministic output.
//!
//! ## Knobs
//!
//! - [`SeedDistribution`] — which subset of nodes starts with the closure
//! - [`BandwidthDistribution`] — how per-edge bandwidth varies
//! - [`FailureSchedule`] — when and where to inject failures during the run
//!
//! Each is a sum-type so callers can mix and match without baking a
//! specific distribution into the simulator.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use consortium_nix::cascade::{CascadeError, NetworkProfile, NodeId, NodeSpec};
use rand::distributions::{Distribution, WeightedIndex};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

// ============================================================================
// Seed distribution — which nodes start with the closure
// ============================================================================

/// How to choose the initial set of nodes that have the closure at
/// cascade start.
#[derive(Debug, Clone)]
pub enum SeedDistribution {
    /// Exactly one node (the build host) starts with the closure.
    /// Always picks `NodeId(0)` for reproducibility.
    Single,
    /// `fraction` of all nodes are pre-seeded, chosen uniformly at random.
    /// `fraction` clamped to `[0, 1]`.
    Random { fraction: f64 },
    /// Exactly `count` nodes pre-seeded (chosen uniformly at random).
    Count(usize),
    /// Caller-specified set of nodes (escape hatch for hand-crafted scenarios).
    Explicit(HashSet<NodeId>),
}

impl SeedDistribution {
    pub fn sample(&self, rng: &mut ChaCha8Rng, n_nodes: u32) -> HashSet<NodeId> {
        match self {
            SeedDistribution::Single => {
                let mut s = HashSet::new();
                if n_nodes > 0 {
                    s.insert(NodeId(0));
                }
                s
            }
            SeedDistribution::Random { fraction } => {
                let f = fraction.clamp(0.0, 1.0);
                let count = ((n_nodes as f64) * f).round() as usize;
                Self::Count(count.max(1).min(n_nodes as usize)).sample(rng, n_nodes)
            }
            SeedDistribution::Count(c) => {
                let count = (*c).min(n_nodes as usize);
                let mut ids: Vec<NodeId> = (0..n_nodes).map(NodeId).collect();
                ids.shuffle(rng);
                ids.into_iter().take(count).collect()
            }
            SeedDistribution::Explicit(s) => s.clone(),
        }
    }
}

// ============================================================================
// Bandwidth distribution
// ============================================================================

/// How per-edge bandwidth varies. Generators populate the
/// [`NetworkProfile::bandwidth`] map; edges absent from the map fall
/// through to the executor's default.
#[derive(Debug, Clone)]
pub enum BandwidthDistribution {
    /// Every edge gets the same bandwidth (bytes/sec).
    Uniform(u64),
    /// Each edge picks from `{slow, fast}` with `fast_fraction` of
    /// edges getting `fast` bandwidth, the rest `slow`.
    Bimodal {
        slow: u64,
        fast: u64,
        fast_fraction: f64,
    },
    /// Each edge sampled from a discrete distribution: `(weight, bytes/sec)`.
    /// Weights are relative.
    Discrete(Vec<(u32, u64)>),
}

impl BandwidthDistribution {
    pub fn populate(&self, rng: &mut ChaCha8Rng, net: &mut NetworkProfile, n_nodes: u32) {
        match self {
            BandwidthDistribution::Uniform(bw) => {
                for src in 0..n_nodes {
                    for tgt in 0..n_nodes {
                        if src == tgt {
                            continue;
                        }
                        net.bandwidth.insert((NodeId(src), NodeId(tgt)), *bw);
                    }
                }
            }
            BandwidthDistribution::Bimodal {
                slow,
                fast,
                fast_fraction,
            } => {
                let f = fast_fraction.clamp(0.0, 1.0);
                for src in 0..n_nodes {
                    for tgt in 0..n_nodes {
                        if src == tgt {
                            continue;
                        }
                        let bw = if rng.gen::<f64>() < f { *fast } else { *slow };
                        net.bandwidth.insert((NodeId(src), NodeId(tgt)), bw);
                    }
                }
            }
            BandwidthDistribution::Discrete(weights) => {
                let dist = WeightedIndex::new(weights.iter().map(|(w, _)| *w))
                    .expect("BandwidthDistribution::Discrete needs at least one weight > 0");
                for src in 0..n_nodes {
                    for tgt in 0..n_nodes {
                        if src == tgt {
                            continue;
                        }
                        let idx = dist.sample(rng);
                        let bw = weights[idx].1;
                        net.bandwidth.insert((NodeId(src), NodeId(tgt)), bw);
                    }
                }
            }
        }
    }
}

// ============================================================================
// Uplink distribution — per-node link capacities
// ============================================================================

/// How to assign per-node link capacities. Generators populate the
/// [`NetworkProfile::nodes`] map, which engages contention modeling in
/// [`NetworkProfile::effective_bandwidth`].
///
/// Nodes absent from the map are treated as having infinite uplink/downlink
/// (degenerate to per-edge bandwidth only — same as the simple model).
#[derive(Debug, Clone)]
pub enum UplinkDistribution {
    /// Every node gets the same uplink. Downlink = 4 × uplink, matching
    /// the asymmetric-link assumption in `NetworkBuilder::uplinks_uniform`.
    Uniform(u64),
    /// Bimodal: data-center nodes (fast) and residential nodes (slow).
    /// `fast_fraction` of nodes get the fast uplink; the rest get slow.
    /// Downlink = 4 × uplink for each tier.
    Bimodal {
        slow: u64,
        fast: u64,
        fast_fraction: f64,
    },
    /// Caller-specified per-node specs (escape hatch for hand-crafted scenarios).
    Explicit(HashMap<NodeId, NodeSpec>),
}

impl UplinkDistribution {
    /// Populate `net.nodes` with per-node specs for `0..n_nodes`.
    pub fn populate(&self, rng: &mut ChaCha8Rng, net: &mut NetworkProfile, n_nodes: u32) {
        match self {
            UplinkDistribution::Uniform(uplink) => {
                for i in 0..n_nodes {
                    net.nodes.insert(
                        NodeId(i),
                        NodeSpec {
                            uplink: *uplink,
                            downlink: uplink.saturating_mul(4),
                        },
                    );
                }
            }
            UplinkDistribution::Bimodal {
                slow,
                fast,
                fast_fraction,
            } => {
                let f = fast_fraction.clamp(0.0, 1.0);
                for i in 0..n_nodes {
                    let uplink = if rng.gen::<f64>() < f { *fast } else { *slow };
                    net.nodes.insert(
                        NodeId(i),
                        NodeSpec {
                            uplink,
                            downlink: uplink.saturating_mul(4),
                        },
                    );
                }
            }
            UplinkDistribution::Explicit(specs) => {
                net.nodes.extend(specs.iter().map(|(k, v)| (*k, *v)));
            }
        }
    }
}

// ============================================================================
// Failure schedule
// ============================================================================

/// Injects deterministic failures into the cascade. The
/// `DeterministicExecutor` calls `failure_for(round, src, tgt)` for
/// every edge it dispatches; non-`None` returns become the edge's
/// outcome.
#[derive(Debug, Clone, Default)]
pub enum FailureSchedule {
    /// No failures.
    #[default]
    None,
    /// Fail every edge whose target is `node`, starting at `round`.
    KillNodeAtRound { node: NodeId, round: u32 },
    /// Fail the specific `(src, tgt)` edge at `round`.
    PartitionAtRound {
        src: NodeId,
        tgt: NodeId,
        round: u32,
    },
    /// Fail each edge with probability `fraction` (0.0 to 1.0). The
    /// outcome is deterministic — same `seed`, `round`, `src`, `tgt`
    /// produce the same fail/succeed decision via a hash of all four,
    /// so reproducing a "weird" failure pattern is just a matter of
    /// re-running with the same seed.
    Random { fraction: f64, seed: u64 },
    /// Fail edges per a hand-built table: `(round, src, tgt) -> Error`.
    Explicit(HashMap<(u32, NodeId, NodeId), CascadeError>),
    /// Multiple failure schedules composed (any one matching = fail).
    All(Vec<FailureSchedule>),
    /// Models a fleet where peer-to-peer SSH isn't configured but
    /// seed → peer SSH works (the nixlab default: dev box has SSH to
    /// every host, but cluster hosts don't trust each other's keys).
    /// Every edge whose `src != seed` returns a transient
    /// `CascadeError::Copy` mimicking real `Permission denied` /
    /// `Host key verification failed` stderr — the cascade should
    /// re-route those targets to seed-source on the next round.
    PeerSshUnconfigured { seed: NodeId },
}

impl FailureSchedule {
    pub fn failure_for(&self, round: u32, src: NodeId, tgt: NodeId) -> Option<CascadeError> {
        match self {
            FailureSchedule::None => None,
            FailureSchedule::KillNodeAtRound { node, round: r } => {
                if round >= *r && tgt == *node {
                    // Permanent failure: the node is dead. Use
                    // Activation error (signals "target node failed
                    // to activate the closure") so the coordinator
                    // marks it in failed_nodes. Orphan re-routing
                    // then kicks in for any descendants in level-tree.
                    Some(CascadeError::Activation {
                        node: tgt,
                        stage: "killed",
                    })
                } else {
                    None
                }
            }
            FailureSchedule::PartitionAtRound {
                src: s,
                tgt: t,
                round: r,
            } => {
                if round >= *r && src == *s && tgt == *t {
                    Some(CascadeError::Partitioned { src, tgt })
                } else {
                    None
                }
            }
            FailureSchedule::Random { fraction, seed } => {
                // Deterministic per-edge decision: mix (seed, round, src,
                // tgt) into a u64 with FxHash-style XOR/multiply, then
                // map to [0, 1) and compare against fraction. Same
                // inputs → same outcome, so a failure pattern is
                // reproducible from the seed. Different rounds get
                // different hashes, so a retry has a fresh chance.
                let mut state: u64 = *seed;
                state ^= (round as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                state ^= (src.0 as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                state ^= (tgt.0 as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
                state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
                state ^= state >> 33;
                // Map to [0.0, 1.0): take high 53 bits (after the >>33
                // mixing has scrambled them), divide by 2^53.
                let p = ((state >> 11) as f64) * (1.0 / ((1u64 << 53) as f64));
                if p < fraction.clamp(0.0, 1.0) {
                    Some(CascadeError::Copy {
                        node: tgt,
                        stderr: format!(
                            "random failure (seed={seed} round={round} src={src} tgt={tgt})"
                        ),
                    })
                } else {
                    None
                }
            }
            FailureSchedule::Explicit(map) => map.get(&(round, src, tgt)).cloned(),
            FailureSchedule::All(schedules) => schedules
                .iter()
                .find_map(|s| s.failure_for(round, src, tgt)),
            FailureSchedule::PeerSshUnconfigured { seed } => {
                if src == *seed {
                    None
                } else {
                    Some(CascadeError::Copy {
                        node: tgt,
                        stderr: format!(
                            "Permission denied (publickey).\n\
                             Host key verification failed.\n\
                             (simulated: peer→peer SSH not configured between {src} and {tgt})"
                        ),
                    })
                }
            }
        }
    }
}

// ============================================================================
// Latency distribution (helper, not first-class)
// ============================================================================

/// Sprinkle uniform latency across all edges. Useful when bandwidth
/// alone isn't enough to differentiate strategies (latency dominates
/// for tiny closures).
pub fn populate_uniform_latency(net: &mut NetworkProfile, latency: Duration, n_nodes: u32) {
    for src in 0..n_nodes {
        for tgt in 0..n_nodes {
            if src == tgt {
                continue;
            }
            net.latency.insert((NodeId(src), NodeId(tgt)), latency);
        }
    }
}

// ============================================================================
// RNG helper
// ============================================================================

/// Build a deterministic ChaCha8Rng from a u64 seed.
pub fn rng_from_seed(seed: u64) -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_distribution_single_returns_node_zero() {
        let mut rng = rng_from_seed(1);
        let s = SeedDistribution::Single.sample(&mut rng, 10);
        assert_eq!(s.len(), 1);
        assert!(s.contains(&NodeId(0)));
    }

    #[test]
    fn seed_distribution_count_clamps_to_n() {
        let mut rng = rng_from_seed(1);
        let s = SeedDistribution::Count(100).sample(&mut rng, 10);
        assert_eq!(s.len(), 10);
    }

    #[test]
    fn seed_distribution_random_is_reproducible() {
        let dist = SeedDistribution::Random { fraction: 0.3 };
        let s1 = dist.sample(&mut rng_from_seed(42), 100);
        let s2 = dist.sample(&mut rng_from_seed(42), 100);
        assert_eq!(s1, s2);
    }

    #[test]
    fn random_failure_schedule_is_deterministic_per_edge() {
        let s = FailureSchedule::Random {
            fraction: 0.5,
            seed: 42,
        };
        // Same (round, src, tgt) → same outcome on repeated calls.
        let a = s.failure_for(3, NodeId(7), NodeId(11));
        let b = s.failure_for(3, NodeId(7), NodeId(11));
        assert_eq!(
            a.is_some(),
            b.is_some(),
            "deterministic per-edge: same query → same outcome"
        );
    }

    #[test]
    fn random_failure_schedule_respects_fraction_at_scale() {
        // Sample 10000 edges at fraction=0.3; expect ~3000 failures.
        let s = FailureSchedule::Random {
            fraction: 0.3,
            seed: 0xc0ffee,
        };
        let mut fails = 0;
        let n_samples = 10_000u32;
        for i in 0..n_samples {
            // Use varied (round, src, tgt) so the hash mixes properly.
            let round = i / 100;
            let src = NodeId(i % 50);
            let tgt = NodeId(i % 47);
            if s.failure_for(round, src, tgt).is_some() {
                fails += 1;
            }
        }
        let observed = fails as f64 / n_samples as f64;
        assert!(
            (observed - 0.3).abs() < 0.03,
            "expected ~30% failure rate; got {observed:.2}% over {n_samples} samples"
        );
    }

    #[test]
    fn random_failure_schedule_at_zero_never_fails() {
        let s = FailureSchedule::Random {
            fraction: 0.0,
            seed: 1,
        };
        for round in 0..10 {
            for i in 0..50 {
                let src = NodeId(i);
                let tgt = NodeId(i + 1);
                assert!(
                    s.failure_for(round, src, tgt).is_none(),
                    "fraction=0.0 should never fail"
                );
            }
        }
    }

    #[test]
    fn random_failure_schedule_at_one_always_fails() {
        let s = FailureSchedule::Random {
            fraction: 1.0,
            seed: 1,
        };
        for round in 0..3 {
            for i in 0..20 {
                let src = NodeId(i);
                let tgt = NodeId(i + 1);
                assert!(
                    s.failure_for(round, src, tgt).is_some(),
                    "fraction=1.0 should always fail"
                );
            }
        }
    }

    #[test]
    fn bimodal_bandwidth_respects_fraction() {
        let mut net = NetworkProfile::default();
        let dist = BandwidthDistribution::Bimodal {
            slow: 1_000_000,
            fast: 1_000_000_000,
            fast_fraction: 0.5,
        };
        let mut rng = rng_from_seed(1);
        dist.populate(&mut rng, &mut net, 50);
        let fast_count = net
            .bandwidth
            .values()
            .filter(|&&v| v == 1_000_000_000)
            .count();
        let total = net.bandwidth.len();
        let frac = fast_count as f64 / total as f64;
        assert!((frac - 0.5).abs() < 0.05, "expected ~0.5, got {frac}");
    }

    #[test]
    fn uniform_uplink_sets_all_nodes() {
        let mut net = NetworkProfile::default();
        let dist = UplinkDistribution::Uniform(1_000_000);
        let mut rng = rng_from_seed(1);
        dist.populate(&mut rng, &mut net, 10);
        assert_eq!(net.nodes.len(), 10, "all 10 nodes should have specs");
        for i in 0..10u32 {
            let spec = net.nodes.get(&NodeId(i)).expect("node should have spec");
            assert_eq!(spec.uplink, 1_000_000);
            assert_eq!(spec.downlink, 4_000_000);
        }
    }

    #[test]
    fn bimodal_uplink_respects_fraction() {
        let mut net = NetworkProfile::default();
        let dist = UplinkDistribution::Bimodal {
            slow: 1_000_000,
            fast: 1_000_000_000,
            fast_fraction: 0.5,
        };
        let mut rng = rng_from_seed(1);
        dist.populate(&mut rng, &mut net, 50);
        let fast_count = net
            .nodes
            .values()
            .filter(|s| s.uplink == 1_000_000_000)
            .count();
        let total = net.nodes.len();
        assert_eq!(total, 50, "all 50 nodes should have specs");
        let frac = fast_count as f64 / total as f64;
        assert!((frac - 0.5).abs() < 0.15, "expected ~0.5, got {frac}");
    }

    #[test]
    fn kill_node_failure_fires_only_at_or_after_round() {
        let s = FailureSchedule::KillNodeAtRound {
            node: NodeId(5),
            round: 2,
        };
        assert!(s.failure_for(0, NodeId(0), NodeId(5)).is_none());
        assert!(s.failure_for(1, NodeId(0), NodeId(5)).is_none());
        assert!(s.failure_for(2, NodeId(0), NodeId(5)).is_some());
        assert!(s.failure_for(3, NodeId(1), NodeId(5)).is_some());
        // wrong target
        assert!(s.failure_for(2, NodeId(0), NodeId(6)).is_none());
    }
}
