//! Cascade closure-distribution primitive.
//!
//! Replaces source-to-each-target serial copy with a log-N peer-to-peer
//! cascade. Once a node has the closure, it joins the source pool and
//! serves the next round of targets. Errors aggregate up the cascade
//! tree so the user-facing error names every failed node with its
//! parent path intact.
//!
//! ## Architecture
//!
//! - [`CascadeNode`] holds per-node identity + parent/children topology
//!   built up as the cascade runs.
//! - [`NetworkProfile`] describes per-edge bandwidth/latency/partition
//!   state. Empty (default) means uniform, well-connected.
//! - [`CascadeStrategy`] is the pluggable decision surface: given the
//!   current cascade state and the network profile, decide which
//!   `(src, tgt)` edges to fire this round. [`Log2FanOut`] is the
//!   baseline; [`crate::cascade_strategies`] hosts cost-aware variants
//!   that pull in petgraph.
//! - [`RoundExecutor`] is how we actually fire those edges. Production
//!   wires it to `nix copy` (one subprocess per edge, std::thread for
//!   in-round parallelism). Sim wires it to deterministic
//!   bandwidth-driven `madsim::time::sleep`.
//! - [`run_cascade`] is the coordinator loop: ask strategy → dispatch
//!   → record outcomes → repeat until convergence or no further
//!   progress.
//!
//! ## Error semantics
//!
//! When a copy fails, the target's parent (the node that tried to
//! serve it) records the error. At the end of each round those errors
//! bubble one level up: the target's parent's own parent receives a
//! [`CascadeError::SubtreeAggregate`] keyed by the failing branch.
//! Sibling failures merge into the same aggregate, so the user-facing
//! error contains the full failed-subtree shape — not "first error
//! wins."

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use thiserror::Error;

// ============================================================================
// Identity + topology
// ============================================================================

/// Opaque identifier for a node participating in the cascade.
///
/// Allocated densely from 0 by [`NodeIdAlloc`]; safe to use as a Vec
/// index when the alloc is the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "n{}", self.0)
    }
}

/// Allocator for NodeId — keeps ids dense and contiguous.
#[derive(Debug, Default)]
pub struct NodeIdAlloc {
    next: u32,
}

impl NodeIdAlloc {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn alloc(&mut self) -> NodeId {
        let id = NodeId(self.next);
        self.next += 1;
        id
    }
}

/// A node in the cascade. `parent` and `children` are populated as
/// the cascade runs — the initial state of every non-seeded node has
/// `parent = None`.
#[derive(Debug, Clone)]
pub struct CascadeNode {
    pub id: NodeId,
    /// `user@host` for ssh-ng, or any opaque address for sim purposes.
    pub addr: String,
    /// Set when this node *received* the closure from its parent
    /// during the cascade. Pre-seeded nodes have `None`.
    pub parent: Option<NodeId>,
    /// Set as the cascade assigns this node to serve targets.
    pub children: Vec<NodeId>,
}

impl CascadeNode {
    pub fn new(id: NodeId, addr: impl Into<String>) -> Self {
        Self {
            id,
            addr: addr.into(),
            parent: None,
            children: Vec::new(),
        }
    }
}

// ============================================================================
// Network profile
// ============================================================================

/// Per-node link capacity. Absent → unbounded (degenerates to per-edge
/// bandwidth only, matching the simple model). Present → contention
/// math kicks in: a source serving N targets in one round shares its
/// uplink N ways.
///
/// Users opt into realism by *describing more of the network*, not by
/// flipping a "mode" — the executor reads `effective_bandwidth` which
/// always returns the right number whether contention is modeled or not.
#[derive(Debug, Clone, Copy)]
pub struct NodeSpec {
    /// Outbound capacity in bytes/sec.
    pub uplink: u64,
    /// Inbound capacity in bytes/sec.
    pub downlink: u64,
}

impl NodeSpec {
    pub fn symmetric(bytes_sec: u64) -> Self {
        Self {
            uplink: bytes_sec,
            downlink: bytes_sec,
        }
    }
}

/// Network properties: per-edge specs + optional per-node link
/// capacities. Empty fields fall back to caller-supplied defaults at
/// lookup time. Strategies that don't care about the network simply
/// ignore this struct.
#[derive(Debug, Default, Clone)]
pub struct NetworkProfile {
    /// bytes/second per directed edge
    pub bandwidth: HashMap<(NodeId, NodeId), u64>,
    /// per-edge latency
    pub latency: HashMap<(NodeId, NodeId), Duration>,
    /// edges that cannot pass traffic at all
    pub partitions: HashSet<(NodeId, NodeId)>,
    /// per-node link capacities. Absent → no contention modeling for
    /// that node (treated as having infinite uplink/downlink).
    pub nodes: HashMap<NodeId, NodeSpec>,
}

impl NetworkProfile {
    pub fn bandwidth_of(&self, src: NodeId, tgt: NodeId, default: u64) -> u64 {
        self.bandwidth.get(&(src, tgt)).copied().unwrap_or(default)
    }

    pub fn latency_of(&self, src: NodeId, tgt: NodeId, default: Duration) -> Duration {
        self.latency.get(&(src, tgt)).copied().unwrap_or(default)
    }

    pub fn is_partitioned(&self, src: NodeId, tgt: NodeId) -> bool {
        self.partitions.contains(&(src, tgt))
    }

    /// Compute the effective bandwidth available on `(src, tgt)` given
    /// how many edges are simultaneously fanning out from `src` and
    /// fanning in to `tgt` this round.
    ///
    /// Returns `min(edge_bw, src.uplink/src_out_count, tgt.downlink/tgt_in_count)`.
    /// If a node has no `NodeSpec`, its term is `u64::MAX` (effectively
    /// no constraint), so the math degenerates to `edge_bw` — same as
    /// the pre-contention behavior.
    ///
    /// Counts are clamped to `>=1` to avoid divide-by-zero (a count of 0
    /// would mean this edge isn't actually scheduled, but defensively
    /// we don't trust the caller).
    pub fn effective_bandwidth(
        &self,
        src: NodeId,
        tgt: NodeId,
        src_out_count: u64,
        tgt_in_count: u64,
        default_edge_bw: u64,
    ) -> u64 {
        let edge_bw = self.bandwidth_of(src, tgt, default_edge_bw);
        let src_share = self
            .nodes
            .get(&src)
            .map(|s| s.uplink / src_out_count.max(1))
            .unwrap_or(u64::MAX);
        let tgt_share = self
            .nodes
            .get(&tgt)
            .map(|t| t.downlink / tgt_in_count.max(1))
            .unwrap_or(u64::MAX);
        edge_bw.min(src_share).min(tgt_share)
    }
}

// ============================================================================
// Strategy + plan
// ============================================================================

/// What a strategy returns for a single round.
#[derive(Debug, Clone, Default)]
pub struct CascadePlan {
    pub round: u32,
    /// `(src_with_closure, tgt_without_closure)` edges to fire this round.
    /// Each source must appear at most once; each target must appear at
    /// most once — the coordinator does not de-duplicate.
    pub assignments: Vec<(NodeId, NodeId)>,
}

/// Read-only snapshot the strategy inspects to plan the next round.
#[derive(Debug)]
pub struct CascadeState<'a> {
    pub nodes: &'a [CascadeNode],
    pub has_closure: &'a HashSet<NodeId>,
    pub round: u32,
    /// Edges already attempted in prior rounds (success or failure).
    /// A strategy must not re-issue these.
    pub attempted: &'a HashSet<(NodeId, NodeId)>,
    /// Nodes that have failed at least once and should be excluded from
    /// further attempts (set by the coordinator after a failed round).
    pub failed_nodes: &'a HashSet<NodeId>,
}

/// Plug-in cascade decision logic.
pub trait CascadeStrategy: Send + Sync {
    /// Pick edges to fire next. Return an empty plan to halt.
    fn next_round(&self, state: &CascadeState, net: &NetworkProfile) -> CascadePlan;
    fn name(&self) -> &'static str;
}

// ============================================================================
// Error tree
// ============================================================================

/// Errors aggregate up the cascade tree: a parent collects its
/// children's errors via [`CascadeError::merge`], producing a
/// [`SubtreeAggregate`](CascadeError::SubtreeAggregate) the next level
/// up consumes.
#[derive(Debug, Clone, Error)]
pub enum CascadeError {
    #[error("copy to {node} failed: {stderr}")]
    Copy { node: NodeId, stderr: String },

    #[error("ssh handshake to {node} failed (from parent {parent})")]
    SshHandshake { node: NodeId, parent: NodeId },

    #[error("activation on {node} failed at stage {stage}")]
    Activation { node: NodeId, stage: &'static str },

    #[error("partition: cannot reach {tgt} from {src}")]
    Partitioned { src: NodeId, tgt: NodeId },

    #[error("subtree rooted at {node}: {} failure(s) below", errors.len())]
    SubtreeAggregate {
        node: NodeId,
        errors: Vec<CascadeError>,
    },
}

impl CascadeError {
    /// Whether the error is "transient" — a single edge failure that
    /// might succeed on retry from a different source. The coordinator
    /// uses this to decide whether to mark a target as permanently
    /// failed (dead-letter) or leave it open for retry:
    ///
    /// - `Copy` → transient (network glitch, transient bandwidth issue)
    /// - `SshHandshake`, `Activation`, `Partitioned` → permanent
    ///   (target node is dead / unreachable / activation failure)
    /// - `SubtreeAggregate` → never used as a per-edge outcome
    pub fn is_transient(&self) -> bool {
        matches!(self, CascadeError::Copy { .. })
    }

    /// Fold a parent's children's errors into a SubtreeAggregate.
    pub fn merge(parent: NodeId, errors: Vec<CascadeError>) -> Self {
        Self::SubtreeAggregate {
            node: parent,
            errors,
        }
    }

    /// Every leaf NodeId that contributed to this error tree.
    pub fn affected_nodes(&self) -> Vec<NodeId> {
        let mut acc = Vec::new();
        self.collect_affected(&mut acc);
        acc
    }

    fn collect_affected(&self, acc: &mut Vec<NodeId>) {
        match self {
            CascadeError::Copy { node, .. }
            | CascadeError::SshHandshake { node, .. }
            | CascadeError::Activation { node, .. } => acc.push(*node),
            CascadeError::Partitioned { tgt, .. } => acc.push(*tgt),
            CascadeError::SubtreeAggregate { errors, .. } => {
                for e in errors {
                    e.collect_affected(acc);
                }
            }
        }
    }

    /// Walk the error tree, yielding (depth, error) for each non-aggregate.
    pub fn walk_leaves(&self, mut f: impl FnMut(usize, &CascadeError)) {
        fn go(err: &CascadeError, depth: usize, f: &mut dyn FnMut(usize, &CascadeError)) {
            match err {
                CascadeError::SubtreeAggregate { errors, .. } => {
                    for e in errors {
                        go(e, depth + 1, f);
                    }
                }
                _ => f(depth, err),
            }
        }
        go(self, 0, &mut f);
    }
}

// ============================================================================
// Round executor abstraction
// ============================================================================

/// Dispatches one round's worth of edges. Production: spawn one
/// `nix copy` subprocess per edge via std::thread. Sim: madsim's task
/// model + deterministic time.
///
/// Implementations must run all edges concurrently (no serial fallback)
/// — the cascade's whole point is in-round parallelism.
pub trait RoundExecutor: Send + Sync {
    fn dispatch(
        &self,
        nodes: &[CascadeNode],
        edges: &[(NodeId, NodeId)],
        net: &NetworkProfile,
    ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>>;
}

// ============================================================================
// Result
// ============================================================================

/// Outcome of a cascade run.
#[derive(Debug)]
pub struct CascadeResult {
    /// Nodes that successfully received the closure (includes pre-seeded).
    pub converged: Vec<NodeId>,
    /// `None` when every non-seeded node converged; `Some` aggregates
    /// every failed branch with full subtree shape preserved.
    pub failed: Option<CascadeError>,
    /// Number of cascade rounds executed (0 if nothing to do).
    pub rounds: u32,
    /// Wall-time of each round (max edge duration in that round).
    pub round_durations: Vec<Duration>,
}

impl CascadeResult {
    pub fn is_success(&self) -> bool {
        self.failed.is_none()
    }
}

// ============================================================================
// Tracing
// ============================================================================

/// One round's worth of state, captured for inspection / replay / UI.
///
/// The cascade emits one of these per round to whatever [`TraceSink`]
/// is wired in. Production passes `None` (zero overhead — the sink is
/// never even constructed). Sim/tests/UI pass a collector.
#[derive(Debug, Clone)]
pub struct RoundSnapshot {
    pub round: u32,
    /// `has_closure` set BEFORE the strategy planned this round.
    pub has_closure_before: HashSet<NodeId>,
    /// What the strategy chose for this round.
    pub plan: CascadePlan,
    /// Outcomes per edge (success → duration, failure → error).
    pub outcomes: HashMap<(NodeId, NodeId), Result<Duration, CascadeError>>,
    /// `has_closure` set AFTER outcomes were applied.
    pub has_closure_after: HashSet<NodeId>,
    /// Parent linkage accumulated so far (cascade tree shape).
    pub parent_chain: HashMap<NodeId, NodeId>,
    /// Round wall-time = max successful edge duration this round.
    pub round_duration: Duration,
}

/// Receives [`RoundSnapshot`]s as the cascade runs. Default impls in
/// [`crate::cascade_trace`] include a `Vec`-collecting recorder + JSON
/// / DOT / ASCII exporters.
///
/// Implementations are called from inside the cascade coordinator loop
/// so they should be cheap. Heavy work (rendering, network IO) belongs
/// downstream — collect snapshots, render after the run completes.
pub trait TraceSink: Send + Sync {
    fn record(&self, snapshot: &RoundSnapshot);
}

// ============================================================================
// Cascade builder — the user-facing entry point
// ============================================================================

/// Builder for a cascade run. Required fields (`nodes`, `seeded`,
/// `network`, `strategy`, `executor`) panic at `.run()` if missing,
/// with a clear message naming the missing field.
///
/// Optional fields:
/// - `.trace(sink)` — engages snapshot recording
/// - `.max_rounds(n)` — sanity bound, default 64
///
/// Realism is opt-in by *describing more of the network* via
/// [`NetworkBuilder::nodes`] (per-node `NodeSpec { uplink, downlink }`)
/// / [`NetworkBuilder::uplinks_uniform`], not by flipping a mode flag
/// here.
///
/// # Example
///
/// ```ignore
/// let result = Cascade::new()
///     .nodes(nodes)
///     .seeded(seeded)
///     .network(net)
///     .strategy(&MaxBottleneckSpanning)
///     .executor(&exec)
///     .run();
/// ```
pub struct Cascade<'a> {
    nodes: Option<Vec<CascadeNode>>,
    seeded: Option<HashSet<NodeId>>,
    network: Option<NetworkProfile>,
    strategy: Option<&'a dyn CascadeStrategy>,
    executor: Option<&'a dyn RoundExecutor>,
    trace: Option<&'a dyn TraceSink>,
    events: Option<&'a dyn crate::cascade_events::EventSink>,
    max_rounds: u32,
}

impl<'a> Default for Cascade<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Cascade<'a> {
    pub fn new() -> Self {
        Self {
            nodes: None,
            seeded: None,
            network: None,
            strategy: None,
            executor: None,
            trace: None,
            events: None,
            max_rounds: 64,
        }
    }

    pub fn nodes(mut self, n: Vec<CascadeNode>) -> Self {
        self.nodes = Some(n);
        self
    }

    pub fn seeded(mut self, s: HashSet<NodeId>) -> Self {
        self.seeded = Some(s);
        self
    }

    pub fn network(mut self, n: NetworkProfile) -> Self {
        self.network = Some(n);
        self
    }

    pub fn strategy(mut self, s: &'a dyn CascadeStrategy) -> Self {
        self.strategy = Some(s);
        self
    }

    pub fn executor(mut self, e: &'a dyn RoundExecutor) -> Self {
        self.executor = Some(e);
        self
    }

    pub fn trace(mut self, t: &'a dyn TraceSink) -> Self {
        self.trace = Some(t);
        self
    }

    /// Wire in a streaming event sink (preferred over `.trace()` for
    /// new code — emits per-edge lifecycle events instead of per-round
    /// snapshots).
    pub fn events(mut self, e: &'a dyn crate::cascade_events::EventSink) -> Self {
        self.events = Some(e);
        self
    }

    pub fn max_rounds(mut self, n: u32) -> Self {
        self.max_rounds = n;
        self
    }

    pub fn run(self) -> CascadeResult {
        let nodes = self.nodes.expect("Cascade::run: missing .nodes(...)");
        let seeded = self.seeded.expect("Cascade::run: missing .seeded(...)");
        let net = self.network.expect("Cascade::run: missing .network(...)");
        let strategy = self.strategy.expect("Cascade::run: missing .strategy(...)");
        let executor = self.executor.expect("Cascade::run: missing .executor(...)");
        run_cascade_with_events(
            nodes,
            seeded,
            net,
            strategy,
            executor,
            self.max_rounds,
            self.trace,
            self.events,
        )
    }
}

// ============================================================================
// NetworkBuilder — describe the network conversationally
// ============================================================================

/// Builder for [`NetworkProfile`]. Each method describes one facet of
/// the network's physics; calling more methods adds more realism.
///
/// Realism is fully opt-in: a `NetworkBuilder::new().build()` produces
/// an empty profile (no contention, no partitions, no per-edge specs)
/// — same as today's default. Adding `.uplinks_uniform(..)` (or `.nodes(..)`
/// for per-node specs) engages contention math automatically.
///
/// # Example
///
/// ```ignore
/// let net = NetworkBuilder::new()
///     .partitions([(NodeId(0), NodeId(7))])
///     .uplinks_uniform(1024 * 1024 * 1024)  // 1 Gbps each
///     .build();
/// ```
#[derive(Debug, Default)]
pub struct NetworkBuilder {
    profile: NetworkProfile,
}

impl NetworkBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set per-edge bandwidth from an arbitrary iterator.
    pub fn bandwidth<I>(mut self, edges: I) -> Self
    where
        I: IntoIterator<Item = ((NodeId, NodeId), u64)>,
    {
        self.profile.bandwidth.extend(edges);
        self
    }

    /// Set per-edge latency from an arbitrary iterator.
    pub fn latency<I>(mut self, edges: I) -> Self
    where
        I: IntoIterator<Item = ((NodeId, NodeId), Duration)>,
    {
        self.profile.latency.extend(edges);
        self
    }

    /// Mark specific `(src, tgt)` edges as unreachable.
    pub fn partitions<I>(mut self, edges: I) -> Self
    where
        I: IntoIterator<Item = (NodeId, NodeId)>,
    {
        self.profile.partitions.extend(edges);
        self
    }

    /// Set per-node link capacity from an arbitrary iterator.
    /// **Engages contention modeling** for the listed nodes.
    pub fn nodes<I>(mut self, specs: I) -> Self
    where
        I: IntoIterator<Item = (NodeId, NodeSpec)>,
    {
        self.profile.nodes.extend(specs);
        self
    }

    /// Convenience: every node `0..n_nodes` gets the same uplink
    /// (and `downlink = uplink * 4` — typical asymmetric link).
    /// **Engages contention modeling.**
    pub fn uplinks_uniform(mut self, n_nodes: u32, uplink: u64) -> Self {
        for i in 0..n_nodes {
            self.profile.nodes.insert(
                NodeId(i),
                NodeSpec {
                    uplink,
                    downlink: uplink.saturating_mul(4),
                },
            );
        }
        self
    }

    pub fn build(self) -> NetworkProfile {
        self.profile
    }
}

// ============================================================================
// Coordinator loop
// ============================================================================

/// Run a cascade end-to-end.
///
/// The cascade halts when any of:
/// - every reachable node has the closure (success)
/// - the strategy returns an empty plan (no further progress possible)
/// - `max_rounds` is reached (sanity bound)
///
/// `seeded` is the set of nodes that already hold the closure at start.
/// Must be non-empty unless `nodes` is also empty.
///
/// Prefer the [`Cascade`] builder for new call sites — this raw fn is
/// kept for backwards compatibility and for the builder's `.run()` to
/// delegate to.
pub fn run_cascade(
    nodes: Vec<CascadeNode>,
    seeded: HashSet<NodeId>,
    net: NetworkProfile,
    strategy: &dyn CascadeStrategy,
    executor: &dyn RoundExecutor,
    max_rounds: u32,
    trace: Option<&dyn TraceSink>,
) -> CascadeResult {
    run_cascade_with_events(
        nodes, seeded, net, strategy, executor, max_rounds, trace, None,
    )
}

/// Like [`run_cascade`] but also emits fine-grained
/// [`CascadeEvent`](crate::cascade_events::CascadeEvent)s through
/// `events`. Use this for live tracing, JSONL persistence, or any
/// consumer that wants per-edge timing rather than per-round
/// aggregates.
///
/// `trace` and `events` are independent — passing both is fine, useful
/// for migrating from the snapshot model to the event model
/// incrementally.
#[allow(clippy::too_many_arguments)]
pub fn run_cascade_with_events(
    mut nodes: Vec<CascadeNode>,
    seeded: HashSet<NodeId>,
    net: NetworkProfile,
    strategy: &dyn CascadeStrategy,
    executor: &dyn RoundExecutor,
    max_rounds: u32,
    trace: Option<&dyn TraceSink>,
    events: Option<&dyn crate::cascade_events::EventSink>,
) -> CascadeResult {
    use crate::cascade_events::{CascadeEvent, Edge};
    use std::time::SystemTime;

    let strategy_name = strategy.name().to_string();

    if let Some(sink) = events {
        let mut seeded_vec: Vec<NodeId> = seeded.iter().copied().collect();
        seeded_vec.sort();
        sink.emit(&CascadeEvent::Started {
            n_nodes: nodes.len() as u32,
            seeded: seeded_vec,
            strategy: strategy_name.clone(),
            at: SystemTime::now(),
        });
    }

    let mut has_closure = seeded.clone();
    let mut attempted: HashSet<(NodeId, NodeId)> = HashSet::new();
    // failed_nodes contains targets that hit a PERMANENT error
    // (Activation, SshHandshake, Partitioned — i.e. the node is
    // genuinely dead/unreachable). Transient Copy errors do NOT mark
    // failed_nodes; the strategy retries from an alternate source.
    let mut failed_nodes: HashSet<NodeId> = HashSet::new();
    let mut round_durations: Vec<Duration> = Vec::new();

    // per-parent error bucket: when round k completes, the parent of
    // each failed target collects that error here. At end of round
    // these get merged into SubtreeAggregates and bubbled one level up
    // by attaching them to the *parent's* parent's bucket next round.
    let mut pending_errors: HashMap<NodeId, Vec<CascadeError>> = HashMap::new();
    // root-level (no-parent) errors — these are pre-seeded sources or
    // the initial seed. Anything that bubbles all the way up lands here.
    let mut root_errors: Vec<CascadeError> = Vec::new();

    let mut round = 0u32;
    while round < max_rounds {
        let state = CascadeState {
            nodes: &nodes,
            has_closure: &has_closure,
            round,
            attempted: &attempted,
            failed_nodes: &failed_nodes,
        };
        let plan = strategy.next_round(&state, &net);
        if plan.assignments.is_empty() {
            break;
        }

        // Snapshot has_closure BEFORE dispatching, for the trace sink.
        let has_closure_before = if trace.is_some() {
            has_closure.clone()
        } else {
            HashSet::new()
        };

        if let Some(sink) = events {
            sink.emit(&CascadeEvent::PlanComputed {
                round,
                assignments: plan.assignments.iter().copied().map(Edge::from).collect(),
            });
            for (src, tgt) in &plan.assignments {
                sink.emit(&CascadeEvent::EdgeStarted {
                    round,
                    src: *src,
                    tgt: *tgt,
                    at: SystemTime::now(),
                });
            }
        }

        // Mark attempted *before* dispatch so a strategy that's called
        // again sees the in-flight edges as already-tried.
        for edge in &plan.assignments {
            attempted.insert(*edge);
        }

        let outcomes = executor.dispatch(&nodes, &plan.assignments, &net);

        if let Some(sink) = events {
            for (src, tgt) in &plan.assignments {
                match outcomes.get(&(*src, *tgt)) {
                    Some(Ok(d)) => sink.emit(&CascadeEvent::EdgeCompleted {
                        round,
                        src: *src,
                        tgt: *tgt,
                        duration: *d,
                    }),
                    Some(Err(e)) => sink.emit(&CascadeEvent::EdgeFailed {
                        round,
                        src: *src,
                        tgt: *tgt,
                        error: e.clone(),
                    }),
                    None => sink.emit(&CascadeEvent::EdgeFailed {
                        round,
                        src: *src,
                        tgt: *tgt,
                        error: CascadeError::Copy {
                            node: *tgt,
                            stderr: format!("executor returned no result for edge {src} -> {tgt}"),
                        },
                    }),
                }
            }
        }

        // Round wall-time = max edge duration.
        let round_wall = outcomes
            .values()
            .filter_map(|r| r.as_ref().ok())
            .copied()
            .max()
            .unwrap_or(Duration::ZERO);
        round_durations.push(round_wall);

        // Apply outcomes to the cascade graph.
        for (src, tgt) in &plan.assignments {
            match outcomes.get(&(*src, *tgt)) {
                Some(Ok(_)) => {
                    has_closure.insert(*tgt);
                    if let Some(node) = nodes.iter_mut().find(|n| n.id == *tgt) {
                        node.parent = Some(*src);
                    }
                    if let Some(srcnode) = nodes.iter_mut().find(|n| n.id == *src) {
                        if !srcnode.children.contains(tgt) {
                            srcnode.children.push(*tgt);
                        }
                    }
                }
                Some(Err(err)) => {
                    // Record parent linkage even on failure so error
                    // bubbling has a path. The src is the parent that
                    // tried to serve this target.
                    if let Some(node) = nodes.iter_mut().find(|n| n.id == *tgt) {
                        node.parent = Some(*src);
                    }
                    // Permanently mark target as failed ONLY for non-
                    // transient errors. CascadeError::Copy = transient
                    // (retry from another source via the strategy's
                    // fallback). Activation / SshHandshake / Partitioned
                    // = permanent (target is dead/unreachable; orphan
                    // re-routing kicks in for its descendants).
                    if !err.is_transient() {
                        failed_nodes.insert(*tgt);
                    }
                    pending_errors.entry(*src).or_default().push(err.clone());
                }
                None => {
                    // Executor dropped the edge — treat the same as a
                    // transient Copy failure (let the strategy retry).
                    let err = CascadeError::Copy {
                        node: *tgt,
                        stderr: format!("executor returned no result for edge {src} -> {tgt}"),
                    };
                    pending_errors.entry(*src).or_default().push(err);
                }
            }
        }

        // Bubble pending errors one level up. Each parent that has
        // collected children-errors this round folds them into a
        // SubtreeAggregate and the aggregate moves to its *grandparent's*
        // bucket for next round (or to root_errors if no grandparent).
        let mut next_pending: HashMap<NodeId, Vec<CascadeError>> = HashMap::new();
        for (parent, errs) in pending_errors.drain() {
            let aggregate = if errs.len() == 1 {
                errs.into_iter().next().unwrap()
            } else {
                CascadeError::merge(parent, errs)
            };
            let grandparent = nodes.iter().find(|n| n.id == parent).and_then(|n| n.parent);
            match grandparent {
                Some(gp) => next_pending.entry(gp).or_default().push(aggregate),
                None => root_errors.push(aggregate),
            }
        }
        pending_errors = next_pending;

        // Emit trace snapshot for this round (if a sink is wired in).
        if let Some(sink) = trace {
            let parent_chain: HashMap<NodeId, NodeId> = nodes
                .iter()
                .filter_map(|n| n.parent.map(|p| (n.id, p)))
                .collect();
            let snapshot = RoundSnapshot {
                round,
                has_closure_before,
                plan: plan.clone(),
                outcomes: outcomes.clone(),
                has_closure_after: has_closure.clone(),
                parent_chain,
                round_duration: round_wall,
            };
            sink.record(&snapshot);
        }

        if let Some(sink) = events {
            let mut has_closure_vec: Vec<NodeId> = has_closure.iter().copied().collect();
            has_closure_vec.sort();
            sink.emit(&CascadeEvent::RoundCompleted {
                round,
                duration: round_wall,
                has_closure: has_closure_vec,
            });
        }

        round += 1;

        // Halt early if everyone converged.
        let all_converged = nodes.iter().all(|n| has_closure.contains(&n.id));
        if all_converged {
            break;
        }
    }

    // Drain any still-pending errors all the way to root.
    while !pending_errors.is_empty() {
        let mut next_pending: HashMap<NodeId, Vec<CascadeError>> = HashMap::new();
        for (parent, errs) in pending_errors.drain() {
            let aggregate = if errs.len() == 1 {
                errs.into_iter().next().unwrap()
            } else {
                CascadeError::merge(parent, errs)
            };
            let grandparent = nodes.iter().find(|n| n.id == parent).and_then(|n| n.parent);
            match grandparent {
                Some(gp) => next_pending.entry(gp).or_default().push(aggregate),
                None => root_errors.push(aggregate),
            }
        }
        pending_errors = next_pending;
    }

    let converged: Vec<NodeId> = nodes
        .iter()
        .filter(|n| has_closure.contains(&n.id))
        .map(|n| n.id)
        .collect();

    // Prune errors whose target eventually converged via retry.
    // Per-round error buckets bubble up regardless of whether the
    // target later succeeded — without pruning, a cascade that
    // recovered via seed-fallback (round 1: peer-source SSH fail
    // transient → round 2: seed-source success) still reports
    // `failed = Some(...)` because the round-1 errors remain in
    // root_errors. Filter them now: anything affecting only nodes
    // in `has_closure` is no longer relevant.
    let pruned_root: Vec<CascadeError> = root_errors
        .into_iter()
        .filter_map(|e| prune_converged(e, &has_closure))
        .collect();

    let failed = match pruned_root.len() {
        0 => None,
        1 => Some(pruned_root.into_iter().next().unwrap()),
        _ => Some(CascadeError::SubtreeAggregate {
            // synthetic root: NodeId(u32::MAX) signals "user / coordinator,
            // no real node corresponds." Strategies and tests should not
            // dereference this.
            node: NodeId(u32::MAX),
            errors: pruned_root,
        }),
    };

    if let Some(sink) = events {
        let failed_count = nodes
            .iter()
            .filter(|n| !has_closure.contains(&n.id))
            .count();
        sink.emit(&CascadeEvent::Finished {
            converged: converged.len(),
            failed: failed_count,
            rounds: round,
        });
    }

    CascadeResult {
        converged,
        failed,
        rounds: round,
        round_durations,
    }
}

/// Walk a `CascadeError` tree and drop any leaf error whose target
/// eventually ended up in `has_closure` (i.e. the retry succeeded).
/// Returns `None` if every leaf was pruned — the entire subtree was
/// transient noise the cascade recovered from.
///
/// SubtreeAggregate with no surviving children collapses to None.
/// SubtreeAggregate with one surviving child collapses to that child
/// (single-error wrapper has no value).
fn prune_converged(err: CascadeError, has_closure: &HashSet<NodeId>) -> Option<CascadeError> {
    match err {
        CascadeError::Copy { node, .. }
        | CascadeError::SshHandshake { node, .. }
        | CascadeError::Activation { node, .. } => {
            if has_closure.contains(&node) {
                None
            } else {
                Some(err)
            }
        }
        CascadeError::Partitioned { tgt, .. } => {
            if has_closure.contains(&tgt) {
                None
            } else {
                Some(err)
            }
        }
        CascadeError::SubtreeAggregate { node, errors } => {
            let surviving: Vec<CascadeError> = errors
                .into_iter()
                .filter_map(|e| prune_converged(e, has_closure))
                .collect();
            match surviving.len() {
                0 => None,
                1 => Some(surviving.into_iter().next().unwrap()),
                _ => Some(CascadeError::SubtreeAggregate {
                    node,
                    errors: surviving,
                }),
            }
        }
    }
}

// ============================================================================
// Log2FanOut — baseline strategy (the trivial case)
// ============================================================================

/// Pairs sources to targets in id-order. Ignores `NetworkProfile`.
/// Each source is used at most once per round, so convergence on
/// uniform topology is ⌈log₂(N - seeded)⌉ rounds.
pub struct Log2FanOut;

impl CascadeStrategy for Log2FanOut {
    fn name(&self) -> &'static str {
        "log2-fanout"
    }

    fn next_round(&self, state: &CascadeState, net: &NetworkProfile) -> CascadePlan {
        let mut sources: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();
        sources.sort();

        let mut targets: Vec<NodeId> = state
            .nodes
            .iter()
            .filter(|n| !state.has_closure.contains(&n.id))
            .filter(|n| !state.failed_nodes.contains(&n.id))
            .map(|n| n.id)
            .collect();
        targets.sort();

        let n = sources.len().min(targets.len());
        let mut assignments = Vec::with_capacity(n);
        let mut used_targets: HashSet<NodeId> = HashSet::new();

        // Greedy pairing in id order, skipping partitioned + already-attempted edges.
        let mut src_iter = sources.iter().copied();
        for tgt in targets.iter().copied() {
            if used_targets.len() >= n {
                break;
            }
            // find next source that isn't partitioned from tgt and hasn't already
            // attempted this edge
            let chosen = loop {
                let Some(src) = src_iter.next() else {
                    break None;
                };
                if net.is_partitioned(src, tgt) {
                    continue;
                }
                if state.attempted.contains(&(src, tgt)) {
                    continue;
                }
                break Some(src);
            };
            if let Some(src) = chosen {
                assignments.push((src, tgt));
                used_targets.insert(tgt);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test executor: success for every edge, returns a fixed duration.
    struct AllSuccessExecutor {
        edge_duration: Duration,
    }

    impl RoundExecutor for AllSuccessExecutor {
        fn dispatch(
            &self,
            _nodes: &[CascadeNode],
            edges: &[(NodeId, NodeId)],
            _net: &NetworkProfile,
        ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
            edges.iter().map(|e| (*e, Ok(self.edge_duration))).collect()
        }
    }

    /// Test executor: fails edges where target id is in `failing`.
    struct FailingTargetExecutor {
        failing: HashSet<NodeId>,
    }

    impl RoundExecutor for FailingTargetExecutor {
        fn dispatch(
            &self,
            _nodes: &[CascadeNode],
            edges: &[(NodeId, NodeId)],
            _net: &NetworkProfile,
        ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
            edges
                .iter()
                .map(|(src, tgt)| {
                    let outcome = if self.failing.contains(tgt) {
                        Err(CascadeError::Copy {
                            node: *tgt,
                            stderr: format!("simulated copy failure to {tgt} (from {src})"),
                        })
                    } else {
                        Ok(Duration::from_millis(10))
                    };
                    ((*src, *tgt), outcome)
                })
                .collect()
        }
    }

    fn make_nodes(n: u32) -> (Vec<CascadeNode>, NodeIdAlloc) {
        let mut alloc = NodeIdAlloc::new();
        let nodes = (0..n)
            .map(|_| {
                let id = alloc.alloc();
                CascadeNode::new(id, format!("user@host-{}", id.0))
            })
            .collect();
        (nodes, alloc)
    }

    #[test]
    fn log2_fanout_converges_in_log2_rounds_uniform() {
        // 16 nodes, seed = node 0 → expect ⌈log₂(16)⌉ = 4 rounds.
        let (nodes, _) = make_nodes(16);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let exec = AllSuccessExecutor {
            edge_duration: Duration::from_millis(10),
        };
        let result = run_cascade(
            nodes.clone(),
            seeded,
            NetworkProfile::default(),
            &Log2FanOut,
            &exec,
            32,
            None,
        );
        assert!(result.is_success(), "failed: {:?}", result.failed);
        assert_eq!(result.converged.len(), 16);
        assert_eq!(result.rounds, 4, "expected 4 rounds, got {}", result.rounds);
    }

    #[test]
    fn log2_fanout_converges_pre_seeded() {
        // 17 nodes, 4 pre-seeded → converges faster than from 1 seed.
        let (nodes, _) = make_nodes(17);
        let seeded: HashSet<NodeId> = (0..4).map(NodeId).collect();
        let exec = AllSuccessExecutor {
            edge_duration: Duration::from_millis(10),
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &Log2FanOut,
            &exec,
            32,
            None,
        );
        assert!(result.is_success());
        assert_eq!(result.converged.len(), 17);
        // 4 → 8 → 16 → 17 (3 rounds)
        assert!(
            result.rounds <= 3,
            "expected <=3 rounds with 4 pre-seeded nodes, got {}",
            result.rounds
        );
    }

    #[test]
    fn no_double_copy() {
        // Track every (src, tgt) the executor sees — must be unique.
        struct AuditingExecutor {
            seen: std::sync::Mutex<HashSet<(NodeId, NodeId)>>,
        }
        impl RoundExecutor for AuditingExecutor {
            fn dispatch(
                &self,
                _nodes: &[CascadeNode],
                edges: &[(NodeId, NodeId)],
                _net: &NetworkProfile,
            ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
                let mut seen = self.seen.lock().unwrap();
                for e in edges {
                    assert!(seen.insert(*e), "duplicate edge dispatched: {:?}", e);
                }
                edges
                    .iter()
                    .map(|e| (*e, Ok(Duration::from_millis(1))))
                    .collect()
            }
        }
        let (nodes, _) = make_nodes(64);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let exec = AuditingExecutor {
            seen: std::sync::Mutex::new(HashSet::new()),
        };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &Log2FanOut,
            &exec,
            32,
            None,
        );
        assert!(result.is_success());
        assert_eq!(result.converged.len(), 64);
    }

    #[test]
    fn failures_bubble_up_through_parent_chain() {
        // 7 nodes. Node 0 seeded. Force node 6 to fail.
        // Cascade tree (Log2FanOut id-order):
        //   round 0: 0->1
        //   round 1: 0->2, 1->3
        //   round 2: 0->4, 1->5, 2->6   <- this fails
        // Node 6's parent is 2, 2's parent is 0 (root).
        // Final error: SubtreeAggregate{node=2, errors=[Copy{node=6}]}
        // — bubbles up to root_errors since 2's parent is None.
        let (nodes, _) = make_nodes(7);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let mut failing = HashSet::new();
        failing.insert(NodeId(6));
        let exec = FailingTargetExecutor { failing };
        let result = run_cascade(
            nodes,
            seeded,
            NetworkProfile::default(),
            &Log2FanOut,
            &exec,
            32,
            None,
        );
        assert!(!result.is_success(), "expected failure");
        let failed = result.failed.expect("failed should be Some");
        let affected = failed.affected_nodes();
        // Node 6 may appear multiple times (one per retry attempt from
        // alternate sources, since the coordinator no longer marks
        // targets permanently failed on a single edge failure). Dedupe.
        let unique: std::collections::HashSet<_> = affected.iter().copied().collect();
        assert_eq!(
            unique.len(),
            1,
            "only one unique failed node expected, got {affected:?}"
        );
        assert!(
            unique.contains(&NodeId(6)),
            "node 6 should be in affected, got {affected:?}"
        );
    }

    #[test]
    fn partitioned_edges_are_skipped() {
        let (nodes, _) = make_nodes(4);
        let mut seeded = HashSet::new();
        seeded.insert(NodeId(0));
        let mut net = NetworkProfile::default();
        net.partitions.insert((NodeId(0), NodeId(1)));
        net.partitions.insert((NodeId(0), NodeId(2)));
        net.partitions.insert((NodeId(0), NodeId(3)));
        let exec = AllSuccessExecutor {
            edge_duration: Duration::from_millis(1),
        };
        let result = run_cascade(nodes, seeded, net, &Log2FanOut, &exec, 16, None);
        // Node 0 can't reach anyone → strategy returns empty plan immediately.
        assert_eq!(result.converged.len(), 1);
        assert_eq!(result.rounds, 0);
    }

    #[test]
    fn empty_cascade_succeeds_trivially() {
        let exec = AllSuccessExecutor {
            edge_duration: Duration::from_millis(1),
        };
        let result = run_cascade(
            vec![],
            HashSet::new(),
            NetworkProfile::default(),
            &Log2FanOut,
            &exec,
            16,
            None,
        );
        assert!(result.is_success());
        assert_eq!(result.rounds, 0);
        assert!(result.converged.is_empty());
    }

    #[test]
    fn cascade_error_merge_preserves_children() {
        let leaf_a = CascadeError::Copy {
            node: NodeId(10),
            stderr: "a".into(),
        };
        let leaf_b = CascadeError::Copy {
            node: NodeId(11),
            stderr: "b".into(),
        };
        let merged = CascadeError::merge(NodeId(2), vec![leaf_a, leaf_b]);
        let affected = merged.affected_nodes();
        assert_eq!(affected, vec![NodeId(10), NodeId(11)]);
    }
}
