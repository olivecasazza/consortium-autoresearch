//! Models nixlab's real-world SSH topology and pins cascade behavior:
//!
//! - **Full mesh** (every host trusts every other host's SSH keys):
//!   the cascade gets canonical `ceil(log_fanout(N))` rounds. This is
//!   the topology we'd LIKE nixlab to have — needs a NixOS module
//!   adding all hosts' public keys to every host's authorized_keys.
//!
//! - **Seed-only SSH** (peer-to-peer SSH unconfigured — nixlab today):
//!   round 0 fan-out from seed to its `fanout` children works. Round
//!   1 attempts peer→peer copies, every one fails with a (simulated)
//!   `Permission denied` / `Host key verification failed` stderr →
//!   classified transient → strategy retries each target from the
//!   seed in round 2. End state: every host converges, no
//!   "deployment failed" — but it's serial-ish from the seed for
//!   rounds beyond the first level.
//!
//! These tests prove the fallback in `LevelTreeFanOut` (skip already-
//! attempted edges, fall back to any alive source) actually carries
//! the cascade to completion when the underlying infra lacks peer
//! SSH. If we ever ship a NixOS change adding the mesh, the FULL-MESH
//! test verifies the cascade still converges canonically; if that
//! change reverts, the SEED-ONLY test catches the regression.

use consortium_fanout_sim::{
    fixtures::FailureSchedule,
    scenario::{Scenario, ScenarioConfig},
};
use consortium_nix::cascade::NodeId;
use consortium_nix::cascade_strategies::LevelTreeFanOut;

#[test]
fn full_mesh_ssh_converges_in_log_n_rounds() {
    // Topology: every (src, tgt) edge usable. With 16 nodes + fanout=2
    // this is the canonical balanced binary tree → 4 rounds.
    let cfg = ScenarioConfig {
        n_nodes: 16,
        failures: FailureSchedule::None,
        ..ScenarioConfig::default()
    };
    let strategy = LevelTreeFanOut::new(2);
    let result = Scenario::new(cfg).run(&strategy);

    assert!(
        result.is_success(),
        "full-mesh SSH: cascade should fully converge; got failed={:?}",
        result.failed
    );
    assert_eq!(result.converged.len(), 16);
    assert_eq!(
        result.rounds, 4,
        "full-mesh + fanout=2 + N=16 should be ⌈log₂(16)⌉=4 rounds"
    );
}

#[test]
fn seed_only_ssh_still_converges_via_fallback() {
    // Topology: every peer→peer edge fails with a transient SSH error;
    // only seed→peer edges work. This is nixlab's reality (mm0X don't
    // trust each other's host keys). The cascade strategy should:
    //   1. Round 0: schedule (seed → child_1), (seed → child_2). Both
    //      succeed.
    //   2. Round 1: schedule (child_1 → grandchild_1), etc. Every
    //      peer-source edge fails with the simulated SSH error
    //      (transient, target NOT marked failed).
    //   3. Round 2: strategy notes those edges in `attempted`, falls
    //      back to "any alive source not in attempted" — the seed.
    //      All grandchildren copy from seed.
    //   4. Round 3: same fallback for great-grandchildren.
    // End state: every host converges, no failures recorded.
    let cfg = ScenarioConfig {
        n_nodes: 8,
        failures: FailureSchedule::PeerSshUnconfigured { seed: NodeId(0) },
        ..ScenarioConfig::default()
    };
    let strategy = LevelTreeFanOut::new(2);
    let result = Scenario::new(cfg).run(&strategy);

    assert!(
        result.is_success(),
        "seed-only SSH: cascade should still fully converge via seed-fallback; \
         got failed={:?}",
        result.failed
    );
    assert_eq!(
        result.converged.len(),
        8,
        "all 8 nodes should converge — peer-SSH-fail must trigger seed-source retry, \
         not permanently fail the targets"
    );
    // Round count is loose here — anywhere from 4 (canonical log) to
    // ~7 (every level needs an extra retry round). The key invariant
    // is "fully converges", not "converges fast". A perf test would
    // pin the exact round count.
    assert!(
        result.rounds <= 8,
        "rounds should still be bounded; got {}",
        result.rounds
    );
}

#[test]
fn seed_only_ssh_round_0_still_fans_out_from_seed() {
    // Within ROUND 0 alone, the cascade IS peer-to-peer fan-out from
    // the seed: with fanout=k the seed serves k children in parallel.
    // This pins that the peer-SSH-fail topology doesn't *prevent*
    // round-0 fan-out — only round-1+ peer-source edges fail.
    let cfg = ScenarioConfig {
        n_nodes: 4,
        failures: FailureSchedule::PeerSshUnconfigured { seed: NodeId(0) },
        ..ScenarioConfig::default()
    };
    let strategy = LevelTreeFanOut::new(3);
    let result = Scenario::new(cfg).run(&strategy);

    assert!(result.is_success());
    assert_eq!(result.converged.len(), 4);
    // 4 nodes, fanout=3 → seed serves 3 children in round 0. Done.
    assert_eq!(
        result.rounds, 1,
        "fanout=3 + N=4 should fit entirely in round 0; got {} rounds",
        result.rounds
    );
}
