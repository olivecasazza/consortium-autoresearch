//! Trace recorder and exporters for cascade runs.
//!
//! [`TraceRecorder`] implements [`TraceSink`] to collect [`RoundSnapshot`]s
//! as the cascade runs. [`CascadeTrace`] wraps the collected snapshots and
//! provides JSON, DOT, and ASCII exporters for inspection and visualization.
//!
//! ## Usage
//!
//! ```ignore
//! let recorder = TraceRecorder::new();
//! let result = Cascade::new()
//!     .nodes(nodes)
//!     .seeded(seeded)
//!     .network(net)
//!     .strategy(&Log2FanOut)
//!     .executor(&exec)
//!     .trace(&recorder)
//!     .run();
//!
//! let trace = CascadeTrace::from_recorder("log2-fanout", 16, &recorder);
//! println!("{}", trace.to_json());
//! println!("{}", trace.to_dot(None));
//! println!("{}", trace.to_ascii(None));
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::cascade::{NodeId, RoundSnapshot, TraceSink};

// ============================================================================
// TraceRecorder
// ============================================================================

/// A [`TraceSink`] that collects [`RoundSnapshot`]s into a `Vec` under a
/// `Mutex`. Safe to share across threads (e.g. via `Arc<TraceRecorder>`).
///
/// Designed to be cheap in the hot path — just push a clone into the
/// locked vec. Heavy rendering goes in [`CascadeTrace`] after the run.
pub struct TraceRecorder {
    snapshots: Mutex<Vec<RoundSnapshot>>,
}

impl Default for TraceRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceRecorder {
    pub fn new() -> Self {
        Self {
            snapshots: Mutex::new(Vec::new()),
        }
    }

    /// Clone out all collected snapshots in round order.
    pub fn snapshots(&self) -> Vec<RoundSnapshot> {
        self.snapshots.lock().unwrap().clone()
    }

    /// Clone out the most recently recorded snapshot, or `None` if empty.
    pub fn last(&self) -> Option<RoundSnapshot> {
        self.snapshots.lock().unwrap().last().cloned()
    }

    /// Number of snapshots recorded so far.
    pub fn len(&self) -> usize {
        self.snapshots.lock().unwrap().len()
    }

    /// Whether no snapshots have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TraceSink for TraceRecorder {
    fn record(&self, snapshot: &RoundSnapshot) {
        self.snapshots.lock().unwrap().push(snapshot.clone());
    }
}

// ============================================================================
// CascadeTrace — full-run wrapper + exporters
// ============================================================================

/// Combines metadata about a cascade run with the collected snapshots for
/// serialization and visualization.
pub struct CascadeTrace {
    pub strategy_name: String,
    pub n_nodes: u32,
    pub snapshots: Vec<RoundSnapshot>,
}

impl CascadeTrace {
    /// Build a `CascadeTrace` from a completed [`TraceRecorder`].
    pub fn from_recorder(
        strategy_name: impl Into<String>,
        n_nodes: u32,
        rec: &TraceRecorder,
    ) -> Self {
        Self {
            strategy_name: strategy_name.into(),
            n_nodes,
            snapshots: rec.snapshots(),
        }
    }

    // -----------------------------------------------------------------------
    // JSON exporter
    // -----------------------------------------------------------------------

    /// Serialize the trace to a JSON string.
    ///
    /// Deterministic: all node sets / maps are sorted by `NodeId.0` before
    /// emitting.
    ///
    /// Schema:
    /// ```json
    /// {
    ///   "strategy": "log2-fanout",
    ///   "n_nodes": 16,
    ///   "rounds": [
    ///     {
    ///       "round": 0,
    ///       "has_closure_before": [0],
    ///       "has_closure_after": [0, 1],
    ///       "plan": [{"src": 0, "tgt": 1}],
    ///       "outcomes": [{"src": 0, "tgt": 1, "ok": true, "duration_ns": 1000000}],
    ///       "parent_chain": [{"node": 1, "parent": 0}],
    ///       "round_duration_ns": 1000000
    ///     }
    ///   ]
    /// }
    /// ```
    pub fn to_json(&self) -> String {
        let mut rounds_arr = Vec::new();

        for snap in &self.snapshots {
            // has_closure_before — sorted
            let mut before: Vec<u32> = snap.has_closure_before.iter().map(|n| n.0).collect();
            before.sort_unstable();

            // has_closure_after — sorted
            let mut after: Vec<u32> = snap.has_closure_after.iter().map(|n| n.0).collect();
            after.sort_unstable();

            // plan — sorted by (src, tgt)
            let mut plan_entries: Vec<(u32, u32)> = snap
                .plan
                .assignments
                .iter()
                .map(|(s, t)| (s.0, t.0))
                .collect();
            plan_entries.sort_unstable();

            // outcomes — sorted by (src, tgt)
            let mut outcome_keys: Vec<(u32, u32)> =
                snap.outcomes.keys().map(|(s, t)| (s.0, t.0)).collect();
            outcome_keys.sort_unstable();

            // parent_chain — sorted by node id
            let mut parent_keys: Vec<(u32, u32)> =
                snap.parent_chain.iter().map(|(n, p)| (n.0, p.0)).collect();
            parent_keys.sort_unstable();

            // Build the JSON object manually using serde_json::Value
            use serde_json::{json, Value};

            let plan_json: Vec<Value> = plan_entries
                .iter()
                .map(|(s, t)| json!({"src": s, "tgt": t}))
                .collect();

            let outcomes_json: Vec<Value> = outcome_keys
                .iter()
                .map(|(s, t)| {
                    let key = (NodeId(*s), NodeId(*t));
                    match snap.outcomes.get(&key) {
                        Some(Ok(dur)) => json!({
                            "src": s,
                            "tgt": t,
                            "ok": true,
                            "duration_ns": dur.as_nanos() as u64,
                        }),
                        Some(Err(err)) => json!({
                            "src": s,
                            "tgt": t,
                            "ok": false,
                            "error": format!("{err}"),
                        }),
                        None => json!({
                            "src": s,
                            "tgt": t,
                            "ok": false,
                            "error": "executor returned no result",
                        }),
                    }
                })
                .collect();

            let parent_json: Vec<Value> = parent_keys
                .iter()
                .map(|(n, p)| json!({"node": n, "parent": p}))
                .collect();

            let round_obj = json!({
                "round": snap.round,
                "has_closure_before": before,
                "has_closure_after": after,
                "plan": plan_json,
                "outcomes": outcomes_json,
                "parent_chain": parent_json,
                "round_duration_ns": snap.round_duration.as_nanos() as u64,
            });

            rounds_arr.push(round_obj);
        }

        let root = serde_json::json!({
            "strategy": self.strategy_name,
            "n_nodes": self.n_nodes,
            "rounds": rounds_arr,
        });

        serde_json::to_string_pretty(&root).expect("serde_json serialization infallible for Value")
    }

    // -----------------------------------------------------------------------
    // DOT exporter
    // -----------------------------------------------------------------------

    /// Render the cascade tree at a given round as a Graphviz DOT string.
    ///
    /// - `None` → use the final (last) snapshot.
    /// - `Some(r)` → find the snapshot whose `round == r`; if not found,
    ///   falls back to the last snapshot.
    ///
    /// Every node in `has_closure_after` becomes a vertex.
    /// Edges come from `parent_chain` (parent → child), labelled by the
    /// round the edge was established (derived by scanning snapshots up to
    /// the chosen round).
    /// Failed nodes (any `Err` outcome in `outcomes` as tgt) get `color=red`.
    /// Pre-seeded nodes (in `has_closure_before` of round 0, if any) get
    /// `shape=box`.
    pub fn to_dot(&self, round: Option<u32>) -> String {
        let Some(snap) = self.select_snapshot(round) else {
            return "digraph cascade {\n}\n".to_string();
        };

        // Seeds = nodes that had closure before round 0 (i.e., no parent in any snapshot)
        let seeds: HashSet<u32> = self
            .snapshots
            .first()
            .map(|s| s.has_closure_before.iter().map(|n| n.0).collect())
            .unwrap_or_default();

        // All converged nodes at this snapshot
        let mut converged: Vec<u32> = snap.has_closure_after.iter().map(|n| n.0).collect();
        converged.sort_unstable();

        // Determine which nodes failed (Err outcomes as target across all snapshots up to this round)
        let failed_nodes: HashSet<u32> = self.collect_failed_nodes(snap.round);

        // Build edge-to-round map from parent_chain across snapshots up to chosen round
        // For each (parent, child) pair, find the first round they appeared together
        let edge_rounds = self.build_edge_round_map(snap.round);

        let mut out = String::new();
        out.push_str("digraph cascade {\n");
        out.push_str("  rankdir=TB;\n");

        // Emit vertices
        for id in &converged {
            let is_seed = seeds.contains(id);
            let is_failed = failed_nodes.contains(id);
            let shape = if is_seed { "box" } else { "ellipse" };
            let label = if is_seed {
                format!("n{id} (seed)")
            } else {
                format!("n{id}")
            };
            if is_failed {
                out.push_str(&format!(
                    "  n{id} [shape={shape}, label=\"{label}\", color=red];\n"
                ));
            } else {
                out.push_str(&format!("  n{id} [shape={shape}, label=\"{label}\"];\n"));
            }
        }

        // Also emit any nodes that failed (may not be in has_closure_after)
        let mut extra_failed: Vec<u32> = failed_nodes
            .iter()
            .filter(|id| !snap.has_closure_after.contains(&NodeId(**id)))
            .copied()
            .collect();
        extra_failed.sort_unstable();
        for id in &extra_failed {
            let label = format!("n{id}");
            out.push_str(&format!(
                "  n{id} [shape=ellipse, label=\"{label}\", color=red];\n"
            ));
        }

        // Emit edges from parent_chain at chosen snapshot (parent -> child)
        let mut parent_entries: Vec<(u32, u32)> = snap
            .parent_chain
            .iter()
            .map(|(child, parent)| (parent.0, child.0))
            .collect();
        parent_entries.sort_unstable();

        for (parent, child) in &parent_entries {
            let r_label = edge_rounds
                .get(&(*parent, *child))
                .map(|r| format!("r{r}"))
                .unwrap_or_else(|| "?".to_string());
            out.push_str(&format!("  n{parent} -> n{child} [label=\"{r_label}\"];\n"));
        }

        out.push_str("}\n");
        out
    }

    // -----------------------------------------------------------------------
    // ASCII exporter
    // -----------------------------------------------------------------------

    /// Render the cascade tree at a given round as an indented ASCII string.
    ///
    /// Uses Unicode box-drawing characters (`├──`, `└──`, `│`) to show
    /// the parent/child hierarchy. Children are sorted by `NodeId.0` for
    /// determinism.
    ///
    /// Example:
    /// ```text
    /// n0 (seed)
    /// ├── n1 (r0)
    /// │   ├── n3 (r1)
    /// │   └── n4 (r2)
    /// └── n2 (r0)
    /// ```
    pub fn to_ascii(&self, round: Option<u32>) -> String {
        let Some(snap) = self.select_snapshot(round) else {
            return String::new();
        };

        // Seeds = nodes in has_closure_before of round 0
        let seeds: HashSet<u32> = self
            .snapshots
            .first()
            .map(|s| s.has_closure_before.iter().map(|n| n.0).collect())
            .unwrap_or_default();

        // Build children map from parent_chain: parent -> sorted children
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (child, parent) in &snap.parent_chain {
            children.entry(parent.0).or_default().push(child.0);
        }
        for v in children.values_mut() {
            v.sort_unstable();
        }

        // Build edge-to-round map
        let edge_rounds = self.build_edge_round_map(snap.round);

        // Find roots: converged nodes with no parent in the parent_chain
        let children_set: HashSet<u32> = snap.parent_chain.keys().map(|n| n.0).collect();
        let mut roots: Vec<u32> = snap
            .has_closure_after
            .iter()
            .map(|n| n.0)
            .filter(|id| !children_set.contains(id))
            .collect();
        roots.sort_unstable();

        // Also include seeds that have no parent (should be same as above in typical run)
        for s in &seeds {
            if !roots.contains(s) && snap.has_closure_after.contains(&NodeId(*s)) {
                roots.push(*s);
            }
        }
        roots.sort_unstable();
        roots.dedup();

        let mut out = String::new();

        // Read-only inputs shared by every level of the recursive render, bundled
        // so `render_node` takes the tree as one argument instead of three.
        struct TreeView<'a> {
            seeds: &'a HashSet<u32>,
            children: &'a HashMap<u32, Vec<u32>>,
            edge_rounds: &'a HashMap<(u32, u32), u32>,
        }

        // Recursive renderer
        fn render_node(
            id: u32,
            prefix: &str,
            is_last: bool,
            tree: &TreeView<'_>,
            parent: Option<u32>,
            out: &mut String,
        ) {
            // Roots (no parent) have no connector and no prefix indent.
            // Non-root nodes always get a connector regardless of depth.
            let connector = if parent.is_none() {
                ""
            } else if is_last {
                "└── "
            } else {
                "├── "
            };

            let label = if tree.seeds.contains(&id) {
                format!("n{id} (seed)")
            } else if let Some(p) = parent {
                let round_label = tree
                    .edge_rounds
                    .get(&(p, id))
                    .map(|r| format!("r{r}"))
                    .unwrap_or_else(|| "?".to_string());
                format!("n{id} ({round_label})")
            } else {
                format!("n{id}")
            };

            out.push_str(prefix);
            out.push_str(connector);
            out.push_str(&label);
            out.push('\n');

            // The child_prefix extends the current prefix:
            // - Root nodes: children start at depth 1 with no inherited indent
            // - Non-root nodes last-child: no vertical bar continuation
            // - Non-root nodes not-last: vertical bar continuation
            let child_prefix = if parent.is_none() {
                // Children of root see empty prefix, but they are NOT roots themselves.
                // We signal non-root via the `parent` arg passed to each child call.
                String::new()
            } else if is_last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}│   ")
            };

            if let Some(kids) = tree.children.get(&id) {
                let n = kids.len();
                for (i, child) in kids.iter().enumerate() {
                    let child_is_last = i == n - 1;
                    render_node(*child, &child_prefix, child_is_last, tree, Some(id), out);
                }
            }
        }

        let tree = TreeView {
            seeds: &seeds,
            children: &children,
            edge_rounds: &edge_rounds,
        };

        let n = roots.len();
        for (i, root) in roots.iter().enumerate() {
            let is_last = i == n - 1;
            render_node(*root, "", is_last, &tree, None, &mut out);
        }

        out
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn select_snapshot(&self, round: Option<u32>) -> Option<&RoundSnapshot> {
        match round {
            None => self.snapshots.last(),
            Some(r) => {
                // Find snapshot with matching round; fall back to last if not found.
                self.snapshots
                    .iter()
                    .find(|s| s.round == r)
                    .or_else(|| self.snapshots.last())
            }
        }
    }

    /// Collect the set of node ids that appeared as a failed target in any
    /// snapshot up to (and including) the given round.
    fn collect_failed_nodes(&self, up_to_round: u32) -> HashSet<u32> {
        let mut failed = HashSet::new();
        for snap in &self.snapshots {
            if snap.round > up_to_round {
                break;
            }
            for ((_, tgt), outcome) in &snap.outcomes {
                if outcome.is_err() {
                    failed.insert(tgt.0);
                }
            }
        }
        failed
    }

    /// Build a map from (parent_id, child_id) → first round that edge appeared
    /// in the parent_chain, scanning snapshots up to the given round.
    fn build_edge_round_map(&self, up_to_round: u32) -> HashMap<(u32, u32), u32> {
        let mut edge_rounds: HashMap<(u32, u32), u32> = HashMap::new();
        for snap in &self.snapshots {
            if snap.round > up_to_round {
                break;
            }
            for (child, parent) in &snap.parent_chain {
                let key = (parent.0, child.0);
                edge_rounds.entry(key).or_insert(snap.round);
            }
        }
        edge_rounds
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cascade::{
        Cascade, CascadeError, CascadeNode, CascadePlan, Log2FanOut, NetworkProfile, NodeId,
        NodeIdAlloc, RoundExecutor, RoundSnapshot,
    };
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::Duration;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    struct AllSuccessExec {
        edge_duration: Duration,
    }

    impl RoundExecutor for AllSuccessExec {
        fn dispatch(
            &self,
            _nodes: &[CascadeNode],
            edges: &[(NodeId, NodeId)],
            _net: &NetworkProfile,
        ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
            edges.iter().map(|e| (*e, Ok(self.edge_duration))).collect()
        }
    }

    fn make_nodes(n: u32) -> Vec<CascadeNode> {
        let mut alloc = NodeIdAlloc::new();
        (0..n)
            .map(|_| {
                let id = alloc.alloc();
                CascadeNode::new(id, format!("host-{}", id.0))
            })
            .collect()
    }

    fn make_snapshot(
        round: u32,
        before: &[u32],
        after: &[u32],
        pairs: &[(u32, u32)],
    ) -> RoundSnapshot {
        let assignments: Vec<(NodeId, NodeId)> = pairs
            .iter()
            .map(|(s, t)| (NodeId(*s), NodeId(*t)))
            .collect();
        let outcomes: HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> = assignments
            .iter()
            .map(|e| (*e, Ok(Duration::from_millis(1))))
            .collect();
        let parent_chain: HashMap<NodeId, NodeId> =
            assignments.iter().map(|(s, t)| (*t, *s)).collect();
        RoundSnapshot {
            round,
            has_closure_before: before.iter().map(|&id| NodeId(id)).collect(),
            plan: CascadePlan { round, assignments },
            outcomes,
            has_closure_after: after.iter().map(|&id| NodeId(id)).collect(),
            parent_chain,
            round_duration: Duration::from_millis(1),
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: recorder collects snapshots in order
    // -----------------------------------------------------------------------

    #[test]
    fn recorder_collects_snapshots_in_order() {
        let nodes = make_nodes(8);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let exec = AllSuccessExec {
            edge_duration: Duration::from_millis(5),
        };
        let recorder = TraceRecorder::new();

        let result = Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(NetworkProfile::default())
            .strategy(&Log2FanOut)
            .executor(&exec)
            .trace(&recorder)
            .run();

        assert!(result.is_success());
        assert_eq!(recorder.len(), result.rounds as usize);

        // Verify ordering: round numbers must be monotonically increasing
        let snaps = recorder.snapshots();
        for (i, snap) in snaps.iter().enumerate() {
            assert_eq!(snap.round, i as u32);
        }
    }

    // -----------------------------------------------------------------------
    // Test 2: to_json round-trips
    // -----------------------------------------------------------------------

    #[test]
    fn to_json_round_trips() {
        let nodes = make_nodes(8);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let exec = AllSuccessExec {
            edge_duration: Duration::from_millis(1),
        };
        let recorder = TraceRecorder::new();

        let result = Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(NetworkProfile::default())
            .strategy(&Log2FanOut)
            .executor(&exec)
            .trace(&recorder)
            .run();

        assert!(result.is_success());

        let trace = CascadeTrace::from_recorder("log2-fanout", 8, &recorder);
        let json_str = trace.to_json();

        // Parse back
        let val: serde_json::Value = serde_json::from_str(&json_str).expect("valid JSON");

        assert_eq!(val["strategy"], "log2-fanout");
        assert_eq!(val["n_nodes"], 8);

        let rounds = val["rounds"].as_array().expect("rounds is array");
        assert_eq!(rounds.len(), result.rounds as usize);

        // Each round should have required fields
        for r in rounds {
            assert!(r["round"].is_number());
            assert!(r["has_closure_before"].is_array());
            assert!(r["has_closure_after"].is_array());
            assert!(r["plan"].is_array());
            assert!(r["outcomes"].is_array());
            assert!(r["parent_chain"].is_array());
            assert!(r["round_duration_ns"].is_number());
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: to_dot includes all converged nodes
    // -----------------------------------------------------------------------

    #[test]
    fn to_dot_includes_all_converged_nodes() {
        let nodes = make_nodes(8);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let exec = AllSuccessExec {
            edge_duration: Duration::from_millis(1),
        };
        let recorder = TraceRecorder::new();

        let result = Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(NetworkProfile::default())
            .strategy(&Log2FanOut)
            .executor(&exec)
            .trace(&recorder)
            .run();

        assert!(result.is_success());

        let trace = CascadeTrace::from_recorder("log2-fanout", 8, &recorder);
        let dot = trace.to_dot(None);

        // Every converged node id should appear in the DOT output
        for node_id in &result.converged {
            let marker = format!("n{}", node_id.0);
            assert!(
                dot.contains(&marker),
                "DOT missing node {marker} — output:\n{dot}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: to_ascii renders tree shape with seed marker
    // -----------------------------------------------------------------------

    #[test]
    fn to_ascii_renders_tree_shape() {
        let recorder = TraceRecorder::new();
        // Inject snapshots manually for a deterministic tree
        let snap0 = make_snapshot(0, &[0], &[0, 1], &[(0, 1)]);
        let snap1 = make_snapshot(1, &[0, 1], &[0, 1, 2, 3], &[(0, 2), (1, 3)]);
        recorder.record(&snap0);
        recorder.record(&snap1);

        let trace = CascadeTrace::from_recorder("test", 4, &recorder);
        let ascii = trace.to_ascii(None);

        assert!(!ascii.is_empty(), "ASCII output should not be empty");
        assert!(
            ascii.contains("seed"),
            "ASCII should contain seed marker: {ascii}"
        );
        // Should contain child nodes
        assert!(ascii.contains("n1"), "Should contain n1: {ascii}");
        assert!(ascii.contains("n2"), "Should contain n2: {ascii}");
        assert!(ascii.contains("n3"), "Should contain n3: {ascii}");
        // Should use box-drawing characters
        assert!(
            ascii.contains('├') || ascii.contains('└'),
            "Should use box-drawing chars: {ascii}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: recorder is thread-safe
    // -----------------------------------------------------------------------

    #[test]
    fn recorder_is_thread_safe() {
        let recorder = Arc::new(TraceRecorder::new());

        let snap_a = make_snapshot(0, &[0], &[0, 1], &[(0, 1)]);
        let snap_b = make_snapshot(1, &[0, 1], &[0, 1, 2], &[(1, 2)]);

        let rec1 = Arc::clone(&recorder);
        let rec2 = Arc::clone(&recorder);

        // Each thread pushes 50 snapshots to the shared recorder
        let t1 = std::thread::spawn(move || {
            for _ in 0..50 {
                rec1.record(&snap_a);
            }
        });
        let t2 = std::thread::spawn(move || {
            for _ in 0..50 {
                rec2.record(&snap_b);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(recorder.len(), 100, "Expected 100 snapshots total");
    }

    // -----------------------------------------------------------------------
    // Test 6: to_dot handles single seed (no rounds)
    // -----------------------------------------------------------------------

    #[test]
    fn to_dot_empty_trace_returns_empty_graph() {
        let recorder = TraceRecorder::new();
        let trace = CascadeTrace::from_recorder("empty", 1, &recorder);
        let dot = trace.to_dot(None);
        assert!(
            dot.contains("digraph cascade"),
            "Should still produce digraph: {dot}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: to_json has deterministic output
    // -----------------------------------------------------------------------

    #[test]
    fn to_json_deterministic() {
        let recorder = TraceRecorder::new();
        // Use a snapshot with multiple nodes to exercise sorting
        let snap = make_snapshot(0, &[0], &[0, 1, 2, 3], &[(0, 1), (0, 2), (0, 3)]);
        recorder.record(&snap);

        let trace = CascadeTrace::from_recorder("det-test", 4, &recorder);
        let json1 = trace.to_json();
        let json2 = trace.to_json();
        assert_eq!(json1, json2, "to_json must be deterministic");

        // Also verify the has_closure_after array is sorted ascending
        let val: serde_json::Value = serde_json::from_str(&json1).unwrap();
        let after = val["rounds"][0]["has_closure_after"].as_array().unwrap();
        let ids: Vec<u64> = after.iter().map(|v| v.as_u64().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "has_closure_after should be sorted ascending");
    }
}
