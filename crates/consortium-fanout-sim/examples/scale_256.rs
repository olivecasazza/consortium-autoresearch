//! Scale demo: 256-node cascade with MaxBottleneckSpanning.
//!
//! Run with:
//!   cargo run --example scale_256 -p consortium-fanout-sim

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use consortium_fanout_sim::{
    fixtures::{rng_from_seed, BandwidthDistribution, FailureSchedule, UplinkDistribution},
    DeterministicExecutor,
};
use consortium_nix::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use consortium_nix::cascade_events::{CascadeEvent, EventSink};
use consortium_nix::cascade_strategies::MaxBottleneckSpanning;

use consortium_cli::tree::{render, NodeStatus, OutputFormat, TreeNode};

// ============================================================================
// Inline EventCollector — collects all events into a Vec
// ============================================================================

struct EventCollector {
    events: Mutex<Vec<CascadeEvent>>,
}

impl EventCollector {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<CascadeEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

impl EventSink for EventCollector {
    fn emit(&self, event: &CascadeEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

// ============================================================================
// OwnedTreeNode — builds a renderable tree from cascade topology
// ============================================================================

struct OwnedTreeNode {
    label: String,
    status: Option<NodeStatus>,
    meta: Vec<(String, String)>,
    children: Vec<OwnedTreeNode>,
}

impl OwnedTreeNode {
    fn leaf(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status: None,
            meta: Vec::new(),
            children: Vec::new(),
        }
    }

    fn with_status(mut self, s: NodeStatus) -> Self {
        self.status = Some(s);
        self
    }

    fn with_meta(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.meta.push((k.into(), v.into()));
        self
    }

    fn push_child(&mut self, child: OwnedTreeNode) {
        self.children.push(child);
    }
}

impl TreeNode for OwnedTreeNode {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn status(&self) -> Option<NodeStatus> {
        self.status.clone()
    }
    fn metadata(&self) -> Vec<(String, String)> {
        self.meta.clone()
    }
    fn children(&self) -> Vec<&dyn TreeNode> {
        self.children.iter().map(|c| c as &dyn TreeNode).collect()
    }
}

// ============================================================================
// Build tree from events
// ============================================================================

/// Extract the cascade tree topology from events.
/// Returns: (parent_map: child -> parent, round_map: child -> round, converged_set)
fn topology_from_events(
    events: &[CascadeEvent],
) -> (HashMap<u32, u32>, HashMap<u32, u32>, HashSet<u32>) {
    let mut parent_map: HashMap<u32, u32> = HashMap::new();
    let mut round_map: HashMap<u32, u32> = HashMap::new();
    let mut converged: HashSet<u32> = HashSet::new();
    let mut current_round = 0u32;
    let mut current_assignments: Vec<(u32, u32)> = Vec::new();

    for ev in events {
        match ev {
            CascadeEvent::Started { seeded, .. } => {
                for s in seeded {
                    converged.insert(s.0);
                }
            }
            CascadeEvent::PlanComputed { round, assignments } => {
                current_round = *round;
                current_assignments = assignments.iter().map(|e| (e.src.0, e.tgt.0)).collect();
            }
            CascadeEvent::EdgeCompleted { src, tgt, .. } => {
                // Record parent relationship
                parent_map.insert(tgt.0, src.0);
                round_map.insert(tgt.0, current_round);
                converged.insert(tgt.0);
                // Also ensure src is in converged
                converged.insert(src.0);
                let _ = current_assignments.iter(); // suppress unused warning
            }
            CascadeEvent::RoundCompleted { has_closure, .. } => {
                for id in has_closure {
                    converged.insert(id.0);
                }
            }
            _ => {}
        }
    }

    (parent_map, round_map, converged)
}

fn build_tree(events: &[CascadeEvent], n_nodes: u32, total_rounds: u32) -> OwnedTreeNode {
    let (parent_map, round_map, converged) = topology_from_events(events);

    // Build children map: parent -> sorted children
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&child, &parent) in &parent_map {
        children_map.entry(parent).or_default().push(child);
    }
    for v in children_map.values_mut() {
        v.sort_unstable();
    }

    // Find roots: converged nodes with no parent
    let child_set: HashSet<u32> = parent_map.keys().copied().collect();
    let mut roots: Vec<u32> = converged
        .iter()
        .copied()
        .filter(|id| !child_set.contains(id))
        .collect();
    roots.sort_unstable();

    let unconverged = n_nodes - converged.len() as u32;

    // Recursive node builder
    fn build_node(
        id: u32,
        parent: Option<u32>,
        round_map: &HashMap<u32, u32>,
        children_map: &HashMap<u32, Vec<u32>>,
        is_seed: bool,
    ) -> OwnedTreeNode {
        let label = if is_seed {
            format!("n{} (seed)", id)
        } else {
            format!("n{}", id)
        };
        let mut node = OwnedTreeNode::leaf(label).with_status(NodeStatus::Ok);

        if let (Some(_p), Some(r)) = (parent, round_map.get(&id)) {
            node = node.with_meta("r", r.to_string());
        }

        if let Some(kids) = children_map.get(&id) {
            for &kid in kids {
                let child_node = build_node(kid, Some(id), round_map, children_map, false);
                node.push_child(child_node);
            }
        }
        node
    }

    // Build a synthetic root that wraps all seed roots
    let mut root = OwnedTreeNode::leaf(format!(
        "256-node cascade, MaxBottleneckSpanning [{} rounds, {} converged{}]",
        total_rounds,
        converged.len(),
        if unconverged > 0 {
            format!(", {} failed", unconverged)
        } else {
            String::new()
        }
    ));

    for seed_id in roots {
        let subtree = build_node(seed_id, None, &round_map, &children_map, true);
        root.push_child(subtree);
    }

    root
}

// ============================================================================
// Per-round convergence histogram from events
// ============================================================================

fn convergence_histogram(events: &[CascadeEvent]) -> Vec<(u32, usize, usize)> {
    let mut histogram: Vec<(u32, usize, usize)> = Vec::new();
    let mut prev_count = 0usize;

    for ev in events {
        if let CascadeEvent::RoundCompleted {
            round, has_closure, ..
        } = ev
        {
            let after = has_closure.len();
            histogram.push((*round, prev_count, after));
            prev_count = after;
        }
    }

    histogram
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    const N: u32 = 256;
    // 0xcafebabe256. Spelled in decimal because 11 hex digits cannot be
    // grouped in twos without changing the value, and `clippy::unusual_byte_
    // groupings` rejects the odd trailing group.
    const SEED: u64 = 13_949_712_720_470;

    println!("=== 256-node cascade, MaxBottleneckSpanning ===");
    println!("Building network profile for {} nodes...", N);

    let mut rng = rng_from_seed(SEED);
    let mut alloc = NodeIdAlloc::new();

    let nodes: Vec<CascadeNode> = (0..N)
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
    .populate(&mut rng, &mut net, N);

    UplinkDistribution::Bimodal {
        slow: 1024 * 1024,        // 1 MB/s uplink
        fast: 1024 * 1024 * 1024, // 1 GB/s uplink
        fast_fraction: 0.3,
    }
    .populate(&mut rng, &mut net, N);

    let exec = DeterministicExecutor::new(100 * 1024 * 1024, FailureSchedule::None);
    let collector = EventCollector::new();

    println!("Running cascade...");
    let result = Cascade::new()
        .nodes(nodes)
        .seeded(seeded)
        .network(net)
        .strategy(&MaxBottleneckSpanning)
        .executor(&exec)
        .events(&collector)
        .max_rounds(64)
        .run();

    let events = collector.take();

    // Summary
    let total_wall: Duration = result.round_durations.iter().sum();
    println!();
    println!("  rounds:    {}", result.rounds);
    println!("  converged: {}/{} nodes", result.converged.len(), N);
    println!(
        "  wall-time: {:.3}s (sum of round durations)",
        total_wall.as_secs_f64()
    );
    println!("  success:   {}", result.is_success());

    // Per-round convergence histogram
    println!();
    println!("--- Per-round convergence ---");
    let histogram = convergence_histogram(&events);
    for (round, before, after) in &histogram {
        println!(
            "  round {:2}: {:4} → {:4} (+{})",
            round,
            before,
            after,
            after - before
        );
    }

    // Build the tree
    let tree = build_tree(&events, N, result.rounds);

    // Tree at depth 3
    println!();
    println!("--- Cascade tree (depth 3) ---");
    let tree3 = render(
        &tree,
        &OutputFormat::Tree {
            max_depth: Some(3),
            color: false,
        },
    );
    println!("{}", tree3);

    // Tree at depth 5
    println!();
    println!("--- Cascade tree (depth 5) ---");
    let tree5 = render(
        &tree,
        &OutputFormat::Tree {
            max_depth: Some(5),
            color: false,
        },
    );
    println!("{}", tree5);

    // Save full trace JSONL to /tmp/scale_256_trace.jsonl
    let jsonl_path = "/tmp/scale_256_trace.jsonl";
    let mut lines = String::new();
    // Re-run to get events for JSONL (we drained the collector above)
    // Instead, we re-collect from events we still have in `events`
    // (we already have them in `events` — the take() returned them)
    for ev in &events {
        if let Ok(line) = serde_json::to_string(ev) {
            lines.push_str(&line);
            lines.push('\n');
        }
    }
    match std::fs::write(jsonl_path, &lines) {
        Ok(_) => println!(
            "Full trace written to {} ({} events, {} bytes)",
            jsonl_path,
            events.len(),
            lines.len()
        ),
        Err(e) => eprintln!("Warning: failed to write trace: {}", e),
    }
}
