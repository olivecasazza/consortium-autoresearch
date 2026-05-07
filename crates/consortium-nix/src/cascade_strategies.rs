//! Cost-aware cascade strategies built on petgraph.
//!
//! [`Log2FanOut`](crate::cascade::Log2FanOut) ignores the network — it
//! pairs sources to targets in id order. That's correct under uniform
//! topology but pessimal when bandwidth varies: the round wall-time is
//! the slowest edge in the round, so a single (fast-source, slow-target)
//! pairing stalls the whole round.
//!
//! These strategies pick edges *with* knowledge of the network:
//!
//! - [`MaxBottleneckSpanning`]: per round, build a bipartite graph of
//!   (sources × targets) weighted by `bandwidth(src, tgt)`. Greedily
//!   pick the highest-bandwidth edges until either every source is
//!   used once or no targets remain. Maximizes the *slowest* edge in
//!   the round (since round time is gated by it).
//!
//! - [`SteinerGreedy`]: same idea but doesn't constrain each source to
//!   one edge per round — keeps picking the next highest-bandwidth
//!   (src-with-closure → target-without-closure) edge until no more
//!   progress. Fires more edges per round; trades off latency vs
//!   bandwidth contention on the source side.

use std::collections::HashSet;

use crate::cascade::{CascadePlan, CascadeState, CascadeStrategy, NetworkProfile, NodeId};

// Default bandwidth used when an edge has no entry in NetworkProfile.
// 100 MB/s is a reasonable LAN baseline — strategies with no bandwidth
// data degenerate to the Log2FanOut ordering.
const DEFAULT_BW_BYTES_SEC: u64 = 100 * 1024 * 1024;

// ============================================================================
// MaxBottleneckSpanning
// ============================================================================

/// Each round: pick edges that maximize the slowest edge in the round.
///
/// Algorithm: enumerate all candidate `(src, tgt, bandwidth(src,tgt))`
/// triples, sort descending by bandwidth, greedily pick edges where
/// neither src nor tgt has been used yet. Each source serves at most
/// one target per round, so the round's wall-time is gated by the
/// slowest assigned edge — and the greedy max-weight pick *maximizes*
/// that bottleneck.
///
/// This is the *max-weight bipartite matching* the cascade wants per
/// round. We use greedy rather than Hungarian/Hopcroft-Karp because
/// (a) greedy is within a 2-approximation of optimal, (b) per-round
/// graphs are small (matching size = O(N) where N is the cascade
/// size), and (c) each round's matching cost amortizes across log(N)
/// rounds — so even O(N²) greedy is cheap relative to the actual
/// `nix copy` work it schedules.
///
/// Note: this is NOT a spanning tree algorithm despite the name — it's
/// a max-weight matching applied iteratively per round. The
/// *aggregate* deployment topology that emerges across all rounds IS
/// a spanning tree (each non-seeded node has exactly one parent),
/// implicitly built up round-by-round.
pub struct MaxBottleneckSpanning;

impl CascadeStrategy for MaxBottleneckSpanning {
    fn name(&self) -> &'static str {
        "max-bottleneck-spanning"
    }

    fn next_round(&self, state: &CascadeState, net: &NetworkProfile) -> CascadePlan {
        let sources: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();
        let targets: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| !state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();

        if sources.is_empty() || targets.is_empty() {
            return CascadePlan::default();
        }

        // Enumerate all candidate edges, sort descending by bandwidth.
        let mut candidates: Vec<(u64, NodeId, NodeId)> = Vec::new();
        for &src in &sources {
            for &tgt in &targets {
                if net.is_partitioned(src, tgt) {
                    continue;
                }
                if state.attempted.contains(&(src, tgt)) {
                    continue;
                }
                let bw = net.bandwidth_of(src, tgt, DEFAULT_BW_BYTES_SEC);
                candidates.push((bw, src, tgt));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        // Greedy max-weight matching: each src and tgt used at most once.
        let mut used_src: HashSet<NodeId> = HashSet::new();
        let mut used_tgt: HashSet<NodeId> = HashSet::new();
        let mut assignments = Vec::new();
        for (_bw, src, tgt) in candidates {
            if used_src.contains(&src) || used_tgt.contains(&tgt) {
                continue;
            }
            assignments.push((src, tgt));
            used_src.insert(src);
            used_tgt.insert(tgt);
        }

        CascadePlan {
            round: state.round,
            assignments,
        }
    }
}

// ============================================================================
// SteinerGreedy
// ============================================================================

/// Greedy max-bandwidth assignment without the one-edge-per-source cap.
///
/// At each round, pick the highest-bandwidth (source, target) edge
/// where the source has the closure and the target doesn't. Repeat
/// until no edges remain that match those criteria.
///
/// This fires *more edges per round* than `MaxBottleneckSpanning` (a
/// fast source can serve multiple targets in one round) but is gated
/// by the source's outbound bandwidth in practice. Use when network
/// is heavily skewed and a few sources dominate capacity.
pub struct SteinerGreedy;

impl CascadeStrategy for SteinerGreedy {
    fn name(&self) -> &'static str {
        "steiner-greedy"
    }

    fn next_round(&self, state: &CascadeState, net: &NetworkProfile) -> CascadePlan {
        let sources: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();
        let targets: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| !state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();

        if sources.is_empty() || targets.is_empty() {
            return CascadePlan::default();
        }

        // Build all candidate (src, tgt, bw) triples, sort descending by bw.
        let mut candidates: Vec<(u64, NodeId, NodeId)> = Vec::new();
        for &src in &sources {
            for &tgt in &targets {
                if net.is_partitioned(src, tgt) {
                    continue;
                }
                if state.attempted.contains(&(src, tgt)) {
                    continue;
                }
                let bw = net.bandwidth_of(src, tgt, DEFAULT_BW_BYTES_SEC);
                candidates.push((bw, src, tgt));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        // Greedy pick: each src may serve N targets, each tgt receives once.
        let mut used_tgt: HashSet<NodeId> = HashSet::new();
        let mut assignments = Vec::new();
        for (_bw, src, tgt) in candidates {
            if used_tgt.contains(&tgt) {
                continue;
            }
            assignments.push((src, tgt));
            used_tgt.insert(tgt);
        }

        CascadePlan {
            round: state.round,
            assignments,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

// ============================================================================
// LevelTreeFanOut — pre-shaped F-ary tree, level-synchronized
// ============================================================================

/// Pre-shapes the deployment as a balanced F-ary tree using BFS / heap-
/// style id assignment: parent of node `i` (i >= 1) is `(i-1) / fanout`.
/// Each round populates exactly one tree level.
///
/// Round 0: seed → its `fanout` direct children (level 1)
/// Round 1: every level-1 node → its `fanout` children (level 2; F²
///   parallel deploys)
/// Round 2: F³ parallel
/// ...
///
/// This matches what users expect from `nh`/`nom`-style visualization:
/// the tree's "level k" is exactly "round k", so renderers show all of
/// L1 spinning together, then complete, then all of L2 spinning, etc.
/// In contrast `MaxBottleneckSpanning` greedy-picks per round, which
/// produces a heavily left-skewed tree where late nodes appear at
/// surprising depths.
///
/// Use this when:
/// - The deployment topology is known up-front (typical for nix push
///   to a fixed fleet)
/// - Visual tree-shape predictability matters more than per-round
///   bandwidth optimization
/// - You want the "L1 deploys to all children at once" semantic
pub struct LevelTreeFanOut {
    pub fanout: u32,
}

impl LevelTreeFanOut {
    pub fn new(fanout: u32) -> Self {
        assert!(fanout >= 1, "LevelTreeFanOut: fanout must be >= 1");
        Self { fanout }
    }

    /// Children of `id` in the heap-style F-ary tree.
    ///
    /// Currently unused by `next_round` (which iterates by target and
    /// walks UP via `alive_ancestor`), but kept for tests + future
    /// use cases that need to enumerate the planned tree-shape.
    #[allow(dead_code)]
    pub(crate) fn tree_children(&self, id: NodeId, n_nodes: u32) -> Vec<NodeId> {
        let base = id.0.checked_mul(self.fanout);
        let Some(base) = base else { return Vec::new() };
        (1..=self.fanout)
            .filter_map(|k| {
                let child = base.checked_add(k)?;
                if child < n_nodes {
                    Some(NodeId(child))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Heap-style parent: parent of `i` (i >= 1) is `(i-1) / fanout`.
    /// Returns `None` for the root (id 0) or invalid math.
    fn tree_parent(&self, id: NodeId) -> Option<NodeId> {
        if id.0 == 0 {
            return None;
        }
        Some(NodeId((id.0 - 1) / self.fanout))
    }

    /// Walk up the heap from `id` looking for an ancestor that has the
    /// closure AND is not in the failed set. Returns:
    ///
    /// - `Some(parent)` when the tree-parent is alive with closure
    ///   (the canonical level-by-level case)
    /// - `Some(grandparent)` when the parent is in `failed_nodes` and
    ///   the grandparent is alive (orphan re-routing)
    /// - `None` when the parent is just "not ready yet" (no closure
    ///   but also not failed — wait for them)
    /// - `None` for the root (id 0, no parent)
    ///
    /// Critical: only walks PAST a parent that's in `failed_nodes`.
    /// A parent that's just not-yet-served is waited on, not skipped
    /// — otherwise round 0 would short-circuit every target directly
    /// to the seed and break the level-by-level reveal.
    fn alive_ancestor<'a>(&self, id: NodeId, state: &CascadeState<'a>) -> Option<NodeId> {
        let mut cur = id;
        while let Some(parent) = self.tree_parent(cur) {
            if state.has_closure.contains(&parent) && !state.failed_nodes.contains(&parent) {
                return Some(parent);
            }
            // Parent isn't usable yet. If it's not failed either,
            // wait for next round — don't skip ahead.
            if !state.failed_nodes.contains(&parent) {
                return None;
            }
            // Parent is permanently failed — try grandparent.
            cur = parent;
        }
        None
    }
}

impl CascadeStrategy for LevelTreeFanOut {
    fn name(&self) -> &'static str {
        "level-tree"
    }

    fn next_round(&self, state: &CascadeState, net: &NetworkProfile) -> CascadePlan {
        let n_nodes = state.nodes.len() as u32;
        let mut assignments = Vec::new();
        let mut used_targets: HashSet<NodeId> = HashSet::new();

        // Iterate over every TARGET that doesn't yet have the closure.
        // For each, find the best alive source: prefer the heap tree-
        // parent (matches the canonical level-by-level shape), but if
        // that parent is dead/failed, walk UP the heap to find an
        // alive ancestor (the orphan re-routing mechanism). This keeps
        // failures localized — one failed node only stalls its OWN
        // serving attempts, not its entire subtree.
        // Pre-compute the set of "alive sources" (any node with the
        // closure that isn't in failed_nodes) for fallback re-routing
        // when the heap ancestor chain is exhausted.
        let mut alive_sources: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();
        alive_sources.sort();

        for tgt_id in 0..n_nodes {
            let tgt = NodeId(tgt_id);
            if state.has_closure.contains(&tgt) {
                continue;
            }
            if state.failed_nodes.contains(&tgt) {
                continue;
            }
            if used_targets.contains(&tgt) {
                continue;
            }

            // Try the heap ancestor chain first (preserves the
            // canonical level-by-level tree shape).
            //
            // We DO filter by `state.attempted` here — once a
            // (src, tgt) edge has been tried, schedule a different
            // src next time even if the failure was transient. This
            // matters for peer-source SSH failures (e.g. nixlab Mac
            // Minis lacking SSH keys to each other): the edge
            // wouldn't succeed on retry, so route around it. The
            // last-ditch fallback below DOES retry attempted edges
            // when no fresh source remains, so genuinely transient
            // failures still get a second chance.
            //
            // Permanent failures filter via `state.failed_nodes`
            // (populated by the coordinator on non-transient errors).
            let mut chosen: Option<NodeId> = None;
            let heap_anc = self.alive_ancestor(tgt, state);
            if let Some(anc) = heap_anc {
                if !net.is_partitioned(anc, tgt) && !state.attempted.contains(&(anc, tgt)) {
                    chosen = Some(anc);
                }
            }

            // Fallback: if heap path unusable (no alive ancestor,
            // edge partitioned, or already-attempted), pick ANY
            // alive source whose edge to tgt hasn't been attempted.
            // Typical case: peer-source failed in round N → round
            // N+1 picks the seed instead.
            if chosen.is_none() && heap_anc.is_some() {
                for &src in &alive_sources {
                    if net.is_partitioned(src, tgt) {
                        continue;
                    }
                    if state.attempted.contains(&(src, tgt)) {
                        continue;
                    }
                    chosen = Some(src);
                    break;
                }
            }

            // Last-ditch: every src has tried tgt, but tgt isn't in
            // failed_nodes (so error was transient). Retry the heap
            // ancestor — it might succeed this round if the failure
            // really was a glitch (substituter slow, network blip).
            // Without this, transient failures permanently strand
            // the target — regression of the original semantics.
            if chosen.is_none() {
                if let Some(anc) = heap_anc {
                    if !net.is_partitioned(anc, tgt) {
                        chosen = Some(anc);
                    }
                }
            }

            let Some(src) = chosen else { continue };
            assignments.push((src, tgt));
            used_targets.insert(tgt);
        }

        CascadePlan {
            round: state.round,
            assignments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::{
        run_cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc, RoundExecutor,
    };
    use std::collections::HashMap;
    use std::time::Duration;

    /// Test executor: edge duration = closure_size / bandwidth.
    /// Used so we can compare strategies on total wall-time.
    struct BandwidthSimExecutor {
        closure_bytes: u64,
        default_bw: u64,
    }

    impl RoundExecutor for BandwidthSimExecutor {
        fn dispatch(
            &self,
            _nodes: &[CascadeNode],
            edges: &[(NodeId, NodeId)],
            net: &NetworkProfile,
        ) -> HashMap<(NodeId, NodeId), Result<Duration, crate::cascade::CascadeError>> {
            edges
                .iter()
                .map(|(src, tgt)| {
                    let bw = net.bandwidth_of(*src, *tgt, self.default_bw);
                    let secs = self.closure_bytes as f64 / bw as f64;
                    let dur =
                        Duration::from_secs_f64(secs) + net.latency_of(*src, *tgt, Duration::ZERO);
                    ((*src, *tgt), Ok(dur))
                })
                .collect()
        }
    }

    fn make_nodes(n: u32) -> Vec<CascadeNode> {
        let mut alloc = NodeIdAlloc::new();
        (0..n)
            .map(|_| {
                let id = alloc.alloc();
                CascadeNode::new(id, format!("user@host-{}", id.0))
            })
            .collect()
    }

    /// Test executor: edges whose tgt is in the `dead` set fail with
    /// CascadeError::Copy. All others succeed in fixed time.
    struct TargetFailExecutor {
        dead: HashSet<NodeId>,
    }

    impl RoundExecutor for TargetFailExecutor {
        fn dispatch(
            &self,
            _nodes: &[CascadeNode],
            edges: &[(NodeId, NodeId)],
            _net: &NetworkProfile,
        ) -> HashMap<(NodeId, NodeId), Result<Duration, crate::cascade::CascadeError>> {
            edges
                .iter()
                .map(|(src, tgt)| {
                    let outcome = if self.dead.contains(tgt) {
                        // Use Activation (permanent) so the coordinator
                        // marks tgt in failed_nodes — orphan re-routing
                        // depends on this signal.
                        let _ = src;
                        Err(crate::cascade::CascadeError::Activation {
                            node: *tgt,
                            stage: "simulated dead target",
                        })
                    } else {
                        Ok(Duration::from_millis(1))
                    };
                    ((*src, *tgt), outcome)
                })
                .collect()
        }
    }

    #[test]
    fn level_tree_walks_up_to_alive_ancestor_for_orphans() {
        // 15-node binary tree:
        //       0
        //     1   2
        //    3 4 5 6
        //   7 8 9 10 11 12 13 14
        //
        // Kill node 1 (left subtree's whole intermediate level): its
        // pre-assigned children (3, 4) become orphans. The walk-up
        // should cause node 0 (root) to adopt them in a later round.
        // Without re-routing, 7..10 (grandchildren) would never get
        // served because their tree-parents 3, 4 never receive the
        // closure.
        //
        // With re-routing: round 0 plans (0→1) (0→2). Round 0 outcomes:
        // (0→1) FAILS (1 is dead), (0→2) succeeds.
        // Round 1: 1 is failed_node, so heap-walk-up finds 0 alive.
        // Plan: (0→3), (0→4) (re-routed orphans), (2→5), (2→6).
        // Round 2: (0→7..14)? No — heap-walk-up for 7 finds parent 3
        // also failed (never got closure → in_progress check). Walk
        // up to 1: also failed. Walk to 0: alive → plan (0→7).
        //
        // Test: verify >= 13 nodes converge (everyone except 1 + maybe
        // 7..10 if they couldn't reach root). Actually with re-routing
        // 0 can serve all of them eventually, so converged should be
        // 14 (n_nodes - killed_node).
        let nodes = make_nodes(15);
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));
        let mut dead = HashSet::new();
        dead.insert(NodeId(1));
        let exec = TargetFailExecutor { dead };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &LevelTreeFanOut::new(2),
            &exec,
            32,
            None,
        );
        // Node 1 + maybe its descendants 3, 4, 7, 8, 9, 10 are at risk.
        // With orphan re-routing, only node 1 itself should fail.
        // Tightened: assert exactly 14 converge (only n1 fails).
        assert_eq!(
            result.converged.len(),
            14,
            "orphan re-routing should serve every node except the dead one; got {} converged (failed_nodes={:?})",
            result.converged.len(),
            result.failed.as_ref().map(|e| e.affected_nodes()),
        );
        assert!(!result.is_success(), "n1 should still appear as failed");
    }

    #[test]
    fn level_tree_alive_ancestor_only_walks_past_failed_nodes() {
        // Verifies the critical "wait for not-ready parents, walk past
        // failed parents" semantic.
        let lt = LevelTreeFanOut::new(2);
        let nodes = make_nodes(15);
        let attempted = HashSet::new();

        // Case 1: only n0 has closure, no failures.
        // alive_ancestor(7): parent is 3, not failed, not ready → wait.
        // Returns None. (Without this, round 0 would short-circuit
        // every node to the seed.)
        let mut has_closure = std::collections::HashSet::new();
        has_closure.insert(NodeId(0));
        let failed_nodes = HashSet::new();
        let state = crate::cascade::CascadeState {
            nodes: &nodes,
            has_closure: &has_closure,
            round: 0,
            attempted: &attempted,
            failed_nodes: &failed_nodes,
        };
        assert_eq!(
            lt.alive_ancestor(NodeId(7), &state),
            None,
            "should wait for parent (3) to be ready, not skip to root"
        );
        // For n1: parent is n0, alive with closure → returns 0.
        assert_eq!(lt.alive_ancestor(NodeId(1), &state), Some(NodeId(0)));
        assert_eq!(lt.alive_ancestor(NodeId(0), &state), None); // root

        // Case 2: n0 alive, n1 failed, n3 not ready. tgt=7.
        // Walk: 7→3 (not failed, not ready → return None).
        let mut failed_nodes2 = HashSet::new();
        failed_nodes2.insert(NodeId(1));
        let state2 = crate::cascade::CascadeState {
            nodes: &nodes,
            has_closure: &has_closure,
            round: 0,
            attempted: &attempted,
            failed_nodes: &failed_nodes2,
        };
        assert_eq!(
            lt.alive_ancestor(NodeId(7), &state2),
            None,
            "n3 isn't failed (yet), should wait — only walk past actually-failed parents"
        );

        // Case 3: n3 IS failed (along with n1), and n0 alive. tgt=7.
        // Walk: 7→3 (failed)→1 (failed)→0 (alive) → returns 0.
        let mut failed_nodes3 = HashSet::new();
        failed_nodes3.insert(NodeId(1));
        failed_nodes3.insert(NodeId(3));
        let state3 = crate::cascade::CascadeState {
            nodes: &nodes,
            has_closure: &has_closure,
            round: 0,
            attempted: &attempted,
            failed_nodes: &failed_nodes3,
        };
        assert_eq!(
            lt.alive_ancestor(NodeId(7), &state3),
            Some(NodeId(0)),
            "should walk past two failed ancestors to reach the alive root"
        );
    }

    /// Skewed network: half the edges are slow (1 MB/s), half are fast (1 GB/s).
    fn skewed_network(n: u32) -> NetworkProfile {
        let mut net = NetworkProfile::default();
        for src in 0..n {
            for tgt in 0..n {
                if src == tgt {
                    continue;
                }
                let bw = if (src + tgt) % 2 == 0 {
                    1024 * 1024 // 1 MB/s slow
                } else {
                    1024 * 1024 * 1024 // 1 GB/s fast
                };
                net.bandwidth.insert((NodeId(src), NodeId(tgt)), bw);
            }
        }
        net
    }

    #[test]
    fn max_bottleneck_correct_on_uniform() {
        // Uniform network → should converge in same round count as Log2FanOut.
        let nodes = make_nodes(16);
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));
        let exec = BandwidthSimExecutor {
            closure_bytes: 1024 * 1024 * 100, // 100 MB
            default_bw: 100 * 1024 * 1024,
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &MaxBottleneckSpanning,
            &exec,
            32,
            None,
        );
        assert!(result.is_success(), "failed: {:?}", result.failed);
        assert_eq!(result.converged.len(), 16);
        assert_eq!(result.rounds, 4);
    }

    #[test]
    fn level_tree_fanout_2_produces_balanced_binary_tree() {
        // 15 nodes, fanout=2, 1 seed → perfect binary heap layout:
        //   L0=1 (n0), L1=2 (n1,n2), L2=4 (n3..n6), L3=8 (n7..n14)
        // Each round populates exactly one level → 3 rounds for 15 nodes.
        // Pins the "round k populates level k" semantic that level-tree
        // exists to provide. Catches regressions where greedy picking
        // sneaks back in or the heap-index math drifts.
        let nodes = make_nodes(15);
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));
        let exec = BandwidthSimExecutor {
            closure_bytes: 1024 * 1024,
            default_bw: 100 * 1024 * 1024,
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &LevelTreeFanOut::new(2),
            &exec,
            32,
            None,
        );
        assert!(result.is_success(), "failed: {:?}", result.failed);
        assert_eq!(result.converged.len(), 15);
        // 15 nodes: L0=1, L1=2, L2=4, L3=8 → 3 rounds (L1 + L2 + L3)
        assert_eq!(
            result.rounds, 3,
            "LevelTreeFanOut(fanout=2) at N=15 should converge in exactly 3 rounds (one per tree level past root); got {}",
            result.rounds,
        );
    }

    #[test]
    fn level_tree_assigns_correct_parents() {
        // Heap layout: parent(i) = (i-1)/fanout for i>=1.
        // Test the tree_children helper directly.
        let lt = LevelTreeFanOut::new(2);
        // node 0's children at fanout=2 are nodes 1, 2
        assert_eq!(lt.tree_children(NodeId(0), 100), vec![NodeId(1), NodeId(2)]);
        // node 1's children: 1*2+1=3, 1*2+2=4
        assert_eq!(lt.tree_children(NodeId(1), 100), vec![NodeId(3), NodeId(4)]);
        // node 7 at N=15: 7*2+1=15 (out of range), 7*2+2=16 (out) → empty
        assert_eq!(lt.tree_children(NodeId(7), 15), vec![]);
        // node 6 at N=15: 13, 14 (both in range)
        assert_eq!(
            lt.tree_children(NodeId(6), 15),
            vec![NodeId(13), NodeId(14)]
        );

        // fanout=3: node 1's children = 4, 5, 6
        let lt3 = LevelTreeFanOut::new(3);
        assert_eq!(
            lt3.tree_children(NodeId(1), 100),
            vec![NodeId(4), NodeId(5), NodeId(6)]
        );
    }

    #[test]
    fn steiner_greedy_converges_in_one_round_on_uniform() {
        // Steiner is uncapped per source. With 1 seed, no contention, no
        // per-edge bandwidth heterogeneity, the seed serves all N-1
        // targets in round 0 and the cascade halts. This test pins that
        // strategy-design behavior — if someone "fixes" Steiner to add a
        // per-source cap, the test fails loudly so the rationale gets
        // re-examined rather than silently changed.
        let nodes = make_nodes(16);
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));
        let exec = BandwidthSimExecutor {
            closure_bytes: 1024 * 1024 * 100,
            default_bw: 100 * 1024 * 1024,
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &SteinerGreedy,
            &exec,
            32,
            None,
        );
        assert!(result.is_success(), "failed: {:?}", result.failed);
        assert_eq!(result.converged.len(), 16);
        assert_eq!(
            result.rounds, 1,
            "Steiner should converge in EXACTLY 1 round on uniform topology with 1 seed (it's uncapped per source). Got {} rounds — strategy semantics changed.",
            result.rounds
        );
    }

    #[test]
    fn max_bottleneck_converges_in_log2_rounds_on_uniform() {
        // MaxBottleneck caps each source to 1 outgoing edge per round, so
        // on uniform topology it converges in EXACTLY ⌈log₂(N)⌉ rounds —
        // same shape as Log2FanOut. Pins the per-source cap behavior; if
        // it regresses, this test names the value that broke.
        let nodes = make_nodes(16);
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));
        let exec = BandwidthSimExecutor {
            closure_bytes: 1024 * 1024 * 100,
            default_bw: 100 * 1024 * 1024,
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &MaxBottleneckSpanning,
            &exec,
            32,
            None,
        );
        assert!(result.is_success(), "failed: {:?}", result.failed);
        assert_eq!(result.converged.len(), 16);
        assert_eq!(
            result.rounds, 4,
            "MaxBottleneck should converge in ⌈log₂(16)⌉=4 rounds on uniform topology (capped at 1 edge/source/round). Got {}",
            result.rounds
        );
    }

    #[test]
    fn max_bottleneck_beats_log2_on_skewed() {
        use crate::cascade::Log2FanOut;
        let n = 16;
        let net = skewed_network(n);
        let exec = BandwidthSimExecutor {
            closure_bytes: 1024 * 1024 * 50, // 50 MB
            default_bw: 100 * 1024 * 1024,
        };
        let mut seeded = std::collections::HashSet::new();
        seeded.insert(NodeId(0));

        let log2 = run_cascade(
            make_nodes(n),
            seeded.clone(),
            net.clone(),
            &Log2FanOut,
            &exec,
            32,
            None,
        );
        let mb = run_cascade(
            make_nodes(n),
            seeded,
            net,
            &MaxBottleneckSpanning,
            &exec,
            32,
            None,
        );

        let log2_total: Duration = log2.round_durations.iter().sum();
        let mb_total: Duration = mb.round_durations.iter().sum();
        assert!(log2.is_success());
        assert!(mb.is_success());
        // Tightened: require MaxBottleneck to be measurably better, not
        // just "no worse". 90% threshold catches the case where the
        // bandwidth-aware strategy degenerates and matches Log2's
        // network-blind choice (the previous `<=` admitted this).
        assert!(
            mb_total.as_secs_f64() <= log2_total.as_secs_f64() * 0.90,
            "MaxBottleneck ({:?}) should be at least 10% faster than Log2FanOut ({:?}) on skewed network — bandwidth-aware pairing isn't paying off",
            mb_total,
            log2_total,
        );
    }
}
