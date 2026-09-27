//! Scale smoke tests: verify cascade convergence bounds at 256 and 1024 nodes.
//!
//! Both tests use MaxBottleneckSpanning with a bimodal bandwidth distribution
//! and a pinned seed for reproducibility.

use std::collections::HashSet;

use consortium_fanout_sim::{
    fixtures::{rng_from_seed, BandwidthDistribution, FailureSchedule, UplinkDistribution},
    DeterministicExecutor,
};
use consortium_nix::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use consortium_nix::cascade_strategies::MaxBottleneckSpanning;

// ============================================================================
// Helpers
// ============================================================================

fn build_run(n_nodes: u32, seed: u64) -> consortium_nix::cascade::CascadeResult {
    let mut rng = rng_from_seed(seed);
    let mut alloc = NodeIdAlloc::new();

    let nodes: Vec<CascadeNode> = (0..n_nodes)
        .map(|_| {
            let id = alloc.alloc();
            CascadeNode::new(id, format!("user@host-{}", id.0))
        })
        .collect();

    let mut seeded = HashSet::new();
    seeded.insert(NodeId(0));

    let mut net = NetworkProfile::default();
    BandwidthDistribution::Bimodal {
        slow: 1024 * 1024,        // 1 MB/s
        fast: 1024 * 1024 * 1024, // 1 GB/s
        fast_fraction: 0.3,
    }
    .populate(&mut rng, &mut net, n_nodes);

    UplinkDistribution::Bimodal {
        slow: 1024 * 1024,
        fast: 1024 * 1024 * 1024,
        fast_fraction: 0.3,
    }
    .populate(&mut rng, &mut net, n_nodes);

    let exec = DeterministicExecutor::new(100 * 1024 * 1024, FailureSchedule::None);

    Cascade::new()
        .nodes(nodes)
        .seeded(seeded)
        .network(net)
        .strategy(&MaxBottleneckSpanning)
        .executor(&exec)
        .max_rounds(64)
        .run()
}

// ============================================================================
// Tests
// ============================================================================

/// 256-node cascade must converge in exactly 8 rounds (that is
/// `ceil(log2(256))`).
///
/// Empirically measured at exactly 8 rounds with the bimodal bandwidth
/// and bimodal uplinks scenario at SEED 0xdeadbeef256. Even with 30%/70%
/// heterogeneity, MaxBottleneckSpanning's greedy max-weight matching
/// hits the log₂ lower bound. If this drifts up to 9+, the strategy's
/// matching has degraded — investigate.
#[test]
fn cascade_at_256_nodes_converges_in_exactly_log2_rounds() {
    const N: u32 = 256;
    // 0xdeadbeef256. Spelled in decimal because 11 hex digits cannot be
    // grouped in twos without changing the value, and `clippy::unusual_byte_
    // groupings` rejects the odd trailing group.
    const SEED: u64 = 15_302_363_378_262;
    const EXPECTED_ROUNDS: u32 = 8; // ⌈log₂(256)⌉

    let result = build_run(N, SEED);

    assert!(
        result.is_success(),
        "cascade failed unexpectedly: {:?}",
        result.failed
    );
    assert_eq!(
        result.converged.len() as u32,
        N,
        "expected all {} nodes to converge, got {}",
        N,
        result.converged.len()
    );
    assert_eq!(
        result.rounds, EXPECTED_ROUNDS,
        "MaxBottleneckSpanning at 256 nodes should converge in exactly {} rounds (⌈log₂(256)⌉) — got {}. Strategy may have regressed.",
        EXPECTED_ROUNDS, result.rounds
    );
    assert_eq!(
        result.round_durations.len() as u32,
        result.rounds,
        "round_durations.len() != rounds"
    );
}

/// 1024-node cascade must converge in EXACTLY ⌈log₂(1024)⌉ = 10 rounds.
///
/// Same rationale as the 256-node test: empirically measured at exactly
/// 10 rounds. Pinning the lower bound catches any per-round degradation
/// the looser `<= 13` bound would have masked.
#[test]
fn cascade_at_1024_nodes_converges_in_exactly_log2_rounds() {
    const N: u32 = 1024;
    const SEED: u64 = 0x_dead_beef_1024;
    const EXPECTED_ROUNDS: u32 = 10; // ⌈log₂(1024)⌉

    let result = build_run(N, SEED);

    assert!(
        result.is_success(),
        "cascade failed unexpectedly: {:?}",
        result.failed
    );
    assert_eq!(
        result.converged.len() as u32,
        N,
        "expected all {} nodes to converge, got {}",
        N,
        result.converged.len()
    );
    assert_eq!(
        result.rounds, EXPECTED_ROUNDS,
        "MaxBottleneckSpanning at 1024 nodes should converge in exactly {} rounds (⌈log₂(1024)⌉) — got {}. Strategy may have regressed.",
        EXPECTED_ROUNDS, result.rounds
    );
    assert_eq!(
        result.round_durations.len() as u32,
        result.rounds,
        "round_durations.len() != rounds"
    );
}
