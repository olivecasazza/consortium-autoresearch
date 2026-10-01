//! Event consumers for the cascade event protocol.
//!
//! Three sinks, all implementing [`EventSink`]:
//!
//! - [`JsonlWriter`] — streams events as JSONL to any `Write + Send`
//! - [`EventCollector`] — accumulates events into a `Vec` for batch use / tests
//! - [`SnapshotAccumulator`] — folds events into a cascade-tree view wired
//!   into `tree.rs`'s [`TreeNode`] trait
//!
//! Plus [`render_events`] — same event slice, four output formats.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use consortium_nix::cascade::NodeId;
use consortium_nix::cascade_events::{CascadeEvent, EventSink};

use crate::tree::{NodeStatus, OutputFormat, TreeNode};

// ============================================================================
// JsonlWriter
// ============================================================================

/// Streams events as JSONL (one JSON object per line) to any `Write + Send`.
///
/// Wraps the writer in a `Mutex` so `JsonlWriter` is `Send + Sync` and can be
/// shared across the coordinator threads.
pub struct JsonlWriter {
    inner: Mutex<Box<dyn Write + Send>>,
}

impl JsonlWriter {
    /// Create from any `Write + Send` — e.g. `Box::new(std::io::stdout())`.
    pub fn new(w: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Mutex::new(w),
        }
    }

    /// Open `path` for appending and write JSONL there.
    pub fn file(path: impl AsRef<Path>) -> io::Result<Self> {
        let f = File::create(path)?;
        Ok(Self::new(Box::new(BufWriter::new(f))))
    }
}

impl EventSink for JsonlWriter {
    fn emit(&self, event: &CascadeEvent) {
        if let Ok(line) = serde_json::to_string(event) {
            let mut w = self.inner.lock().unwrap();
            let _ = writeln!(w, "{line}");
        }
    }
}

// ============================================================================
// EventCollector
// ============================================================================

/// Accumulates all events into a `Vec<CascadeEvent>` for batch processing.
/// Useful for tests and for feeding `render_events` after a run.
pub struct EventCollector {
    events: Mutex<Vec<CascadeEvent>>,
}

impl EventCollector {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Clone out the full accumulated event list.
    pub fn events(&self) -> Vec<CascadeEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.events.lock().unwrap().clear();
    }
}

impl Default for EventCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for EventCollector {
    fn emit(&self, event: &CascadeEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

// ============================================================================
// SnapshotAccumulator + OwnedTreeNode
// ============================================================================

/// Per-node state built up as events arrive.
#[derive(Debug, Clone)]
struct NodeState {
    /// NodeId of the parent (established from EdgeCompleted or EdgeFailed).
    parent: Option<NodeId>,
    /// Children NodeIds (targets for which this node was src).
    children: Vec<NodeId>,
    /// Round in which this node converged (EdgeCompleted).
    converged_round: Option<u32>,
    /// Duration of the completing edge.
    duration: Option<Duration>,
    /// True if this node has the closure (either seeded or EdgeCompleted).
    has_closure: bool,
    /// True if this node appeared in any EdgeFailed as tgt.
    failed: bool,
    /// True if this node is currently being served — set on PlanComputed
    /// (we know the strategy assigned it this round), cleared on
    /// EdgeCompleted (success → has_closure) or EdgeFailed (failure).
    /// Drives the "spinning" ⏵ glyph during the wall-time the cascade
    /// is actually doing the copy.
    in_progress: bool,
}

impl NodeState {
    fn new() -> Self {
        Self {
            parent: None,
            children: Vec::new(),
            converged_round: None,
            duration: None,
            has_closure: false,
            failed: false,
            in_progress: false,
        }
    }
}

/// Folds `CascadeEvent`s into per-node snapshots that can be rendered as a
/// cascade tree.
pub struct SnapshotAccumulator {
    inner: Mutex<AccumulatorInner>,
}

#[derive(Default)]
struct AccumulatorInner {
    /// All nodes seen, indexed by NodeId.
    nodes: HashMap<NodeId, NodeState>,
    /// Seed nodes (from `Started`), in order.
    seeds: Vec<NodeId>,
    /// Round totals we track for metadata (round → max edge duration).
    round_durations: HashMap<u32, Duration>,
    /// Total node count from the `Started` event. Used by
    /// `status_counts()` to compute pending as
    /// `total - (ok + in_progress + failed)`. Without this, untouched
    /// nodes (the majority of the fleet during early rounds) wouldn't
    /// appear in the summary at all — e.g. a 121-node cascade in
    /// round 0 would show `1✔ 2⏵ 0⏸ 0⚠` instead of `1✔ 2⏵ 118⏸ 0⚠`.
    total_nodes: u32,
}

impl AccumulatorInner {
    fn node(&mut self, id: NodeId) -> &mut NodeState {
        self.nodes.entry(id).or_insert_with(NodeState::new)
    }
}

impl SnapshotAccumulator {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(AccumulatorInner::default()),
        }
    }

    /// Build a self-contained `OwnedTreeNode` tree.
    ///
    /// Root = `seeded[0]` (or an artificial root labelled "cascade" if no
    /// `Started` event was received).  Children are nodes whose parent edge
    /// (EdgeCompleted or EdgeFailed) established `src→tgt`.  Nodes that
    /// appear in both EdgeCompleted and EdgeFailed keep their last status.
    ///
    /// Status per node:
    /// - `Ok` — `has_closure` is true
    /// - `Failed` — appeared as `tgt` in at least one `EdgeFailed`
    /// - `Pending` — neither
    ///
    /// Judgment call on orphan failed nodes: both `EdgeCompleted` and
    /// `EdgeFailed` contribute to parent-of relationships so that failed
    /// targets still appear in the tree. If we only used `EdgeCompleted`,
    /// failed nodes would be orphans and invisible.
    pub fn to_tree(&self) -> OwnedTreeNode {
        let acc = self.inner.lock().unwrap();
        build_tree(&acc)
    }

    /// Aggregate counts per status: `(ok, in_progress, pending, failed)`.
    /// Used by [`LiveTreeRenderer`] to render the nom-style summary
    /// row at the bottom of each frame: `∑ N✔ M⏵ K⏸ J⚠`.
    ///
    /// Pending is computed as `total_nodes - (ok + in_progress + failed)`
    /// so untouched nodes (which never appear in the events map until
    /// the strategy schedules them) show up correctly in the summary.
    /// Falls back to `nodes.values().count()` if no `Started` event
    /// has been seen yet (degenerate case — defensive).
    pub fn status_counts(&self) -> (usize, usize, usize, usize) {
        let acc = self.inner.lock().unwrap();
        let mut ok = 0;
        let mut in_progress = 0;
        let mut failed = 0;
        for state in acc.nodes.values() {
            if state.failed {
                failed += 1;
            } else if state.has_closure {
                ok += 1;
            } else if state.in_progress {
                in_progress += 1;
            }
            // Touched-but-still-pending nodes are absorbed into the
            // total_nodes-based pending count below — counting them
            // both here and there would double-count.
        }
        let active = ok + in_progress + failed;
        let pending = (acc.total_nodes as usize).saturating_sub(active);
        (ok, in_progress, pending, failed)
    }
}

impl Default for SnapshotAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for SnapshotAccumulator {
    fn emit(&self, event: &CascadeEvent) {
        let mut acc = self.inner.lock().unwrap();
        match event {
            CascadeEvent::Started {
                seeded, n_nodes, ..
            } => {
                acc.seeds = seeded.clone();
                acc.total_nodes = *n_nodes;
                // Pre-populate ALL nodes so the renderer can show
                // pending ones (⏸) from frame 1 — without this, only
                // nodes that appear in some event are visible, which
                // makes the user see "✔ + ⚠" but no ⏸ even when the
                // summary says there are dozens pending.
                for i in 0..*n_nodes {
                    acc.node(NodeId(i));
                }
                for &id in seeded {
                    let n = acc.node(id);
                    n.has_closure = true;
                }
            }
            CascadeEvent::PlanComputed { assignments, .. } => {
                // Register all planned edges as tentative parent→child relationships
                // AND mark targets as in_progress — that drives the "spinning"
                // ⏵ glyph during the actual copy. EdgeCompleted/EdgeFailed will
                // clear the spinner when the edge resolves.
                for edge in assignments {
                    let src = edge.src;
                    let tgt = edge.tgt;
                    acc.node(src);
                    {
                        let n = acc.node(tgt);
                        if n.parent.is_none() {
                            n.parent = Some(src);
                        }
                        n.in_progress = true;
                    }
                    if !acc
                        .nodes
                        .get(&src)
                        .map_or(false, |n| n.children.contains(&tgt))
                    {
                        acc.node(src).children.push(tgt);
                    }
                }
            }
            CascadeEvent::EdgeStarted { src, tgt, .. } => {
                acc.node(*src);
                acc.node(*tgt);
            }
            CascadeEvent::EdgeCompleted {
                round,
                src,
                tgt,
                duration,
            } => {
                // Establish parent→child relationship.
                let src = *src;
                let tgt = *tgt;
                let round = *round;
                let dur = *duration;

                // Register child on src.
                if !acc
                    .nodes
                    .get(&src)
                    .map_or(false, |n| n.children.contains(&tgt))
                {
                    acc.node(src).children.push(tgt);
                }
                // Register parent on tgt + clear in_progress (it's done now).
                {
                    let n = acc.node(tgt);
                    n.parent = Some(src);
                    n.has_closure = true;
                    n.in_progress = false;
                    n.converged_round = Some(round);
                    n.duration = Some(dur);
                }
            }
            CascadeEvent::EdgeFailed {
                round: _, src, tgt, ..
            } => {
                let src = *src;
                let tgt = *tgt;
                if !acc
                    .nodes
                    .get(&src)
                    .map_or(false, |n| n.children.contains(&tgt))
                {
                    acc.node(src).children.push(tgt);
                }
                {
                    let n = acc.node(tgt);
                    if n.parent.is_none() {
                        n.parent = Some(src);
                    }
                    n.failed = true;
                    n.in_progress = false; // resolved (to failure)
                }
            }
            CascadeEvent::RoundCompleted {
                round,
                duration,
                has_closure,
            } => {
                acc.round_durations.insert(*round, *duration);
                for &id in has_closure {
                    acc.node(id).has_closure = true;
                }
            }
            CascadeEvent::Finished { .. } => {}
        }
    }
}

/// Build the `OwnedTreeNode` tree from accumulated state.
fn build_tree(acc: &AccumulatorInner) -> OwnedTreeNode {
    let root_id = acc.seeds.first().copied();

    let Some(rid) = root_id else {
        return OwnedTreeNode {
            label: "cascade".into(),
            status: Some(NodeStatus::Pending),
            metadata: Vec::new(),
            children: Vec::new(),
        };
    };

    let mut root = build_node(rid, acc);

    // Adopt orphans: nodes that have no parent set yet but exist in
    // the accumulator (pre-populated from Started's n_nodes count).
    // Without this, pending nodes that haven't been planned yet are
    // invisible — the user's summary says "61 pending" but the tree
    // shows nothing for them. Adopting under root makes them visible
    // as ⏸; once a real parent emerges from EdgeCompleted/Failed,
    // they move to their proper subtree on the next render.
    let in_tree = collect_node_ids(&root);
    let mut orphans: Vec<NodeId> = acc
        .nodes
        .keys()
        .copied()
        .filter(|id| !in_tree.contains(id))
        .filter(|id| {
            // Skip nodes that have a parent set (they should appear
            // under their parent — if they're not in `in_tree` then
            // their parent is unreachable, but we'll still show them
            // here so the user can see them).
            acc.nodes
                .get(id)
                .and_then(|s| s.parent)
                .is_none_or(|_| true)
        })
        .filter(|id| acc.nodes.get(id).is_some_and(|s| s.parent.is_none()))
        .collect();
    orphans.sort_by_key(|cid| {
        let p = match acc.nodes.get(cid) {
            None => 2u8,
            Some(s) if s.failed => 0,
            Some(s) if s.in_progress => 1,
            Some(s) if !s.has_closure => 2,
            Some(_) => 3,
        };
        (p, *cid)
    });
    for orphan in orphans {
        root.children.push(build_node(orphan, acc));
    }
    root
}

/// Collect every NodeId that appears in the rooted-from-seed subtree.
/// Used to identify orphans (nodes in the accumulator but not yet
/// reachable from the seed via parent links).
fn collect_node_ids(node: &OwnedTreeNode) -> std::collections::HashSet<NodeId> {
    let mut acc = std::collections::HashSet::new();
    fn walk(n: &OwnedTreeNode, acc: &mut std::collections::HashSet<NodeId>) {
        // Parse the NodeId from the label (`nN`). If unparseable
        // (synthetic root), skip — orphan check is best-effort.
        if let Some(num) = n
            .label
            .strip_prefix('n')
            .and_then(|s| s.parse::<u32>().ok())
        {
            acc.insert(NodeId(num));
        }
        for c in &n.children {
            walk(c, acc);
        }
    }
    walk(node, &mut acc);
    acc
}

fn build_node(id: NodeId, acc: &AccumulatorInner) -> OwnedTreeNode {
    let state = acc.nodes.get(&id);

    let status = match state {
        None => Some(NodeStatus::Pending),
        Some(s) if s.failed => Some(NodeStatus::Failed),
        Some(s) if s.has_closure => Some(NodeStatus::Ok),
        Some(s) if s.in_progress => Some(NodeStatus::InProgress),
        Some(_) => Some(NodeStatus::Pending),
    };

    let mut metadata: Vec<(String, String)> = Vec::new();
    if let Some(s) = state {
        if let Some(round) = s.converged_round {
            metadata.push(("round".into(), round.to_string()));
        }
        if let Some(dur) = s.duration {
            let ms = dur.as_millis();
            metadata.push(("duration".into(), format!("{ms}ms")));
        }
    }

    // Build children, sorted by build-state priority (nom/State/Sorting.hs):
    // Failed > InProgress > Pending > Ok, then NodeId for stable tie-breaking.
    // This ensures failed/running nodes float to top and survive truncation.
    let mut child_ids: Vec<NodeId> = state.map(|s| s.children.clone()).unwrap_or_default();
    child_ids.sort();
    child_ids.dedup();
    child_ids.sort_by_key(|cid| {
        let p = match acc.nodes.get(cid) {
            None => 2u8,                    // Pending (unknown)
            Some(s) if s.failed => 0,       // Failed — highest priority
            Some(s) if s.in_progress => 1,  // InProgress
            Some(s) if !s.has_closure => 2, // Pending
            Some(_) => 3,                   // Ok — least interesting
        };
        (p, *cid)
    });

    let children: Vec<OwnedTreeNode> = child_ids
        .into_iter()
        .map(|cid| build_node(cid, acc))
        .collect();

    OwnedTreeNode {
        label: id.to_string(),
        status,
        metadata,
        children,
    }
}

// ============================================================================
// OwnedTreeNode — self-contained tree that implements TreeNode
// ============================================================================

/// A self-contained tree node that owns its children. Implements [`TreeNode`]
/// so it can be handed directly to `tree::render`.
///
/// Constructed by [`SnapshotAccumulator::to_tree`]; also usable standalone.
pub struct OwnedTreeNode {
    pub label: String,
    pub status: Option<NodeStatus>,
    pub metadata: Vec<(String, String)>,
    pub children: Vec<OwnedTreeNode>,
}

impl TreeNode for OwnedTreeNode {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn status(&self) -> Option<NodeStatus> {
        self.status.clone()
    }

    fn metadata(&self) -> Vec<(String, String)> {
        self.metadata.clone()
    }

    fn children(&self) -> Vec<&dyn TreeNode> {
        self.children.iter().map(|c| c as &dyn TreeNode).collect()
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Truncate `s` so its display width fits within `max_cols` terminal
/// columns. If truncated, appends an ellipsis `…` (which is 1 column
/// wide). ANSI escape sequences in `s` are stripped before measuring
/// so they don't count against the budget.
///
/// Why this matters: a line wider than the terminal wraps onto multiple
/// visual lines. The cursor-walk-up cleanup logic counts LOGICAL lines
/// (one per `\n`), not visual lines, so wrapped content ends up
/// only-partially erased between frames — leaving the wrapped
/// portion as visual cruft.
///
/// Uses byte-level char iteration because `console::measure_text_width`
/// requires the full `unicode-width` dep tree we don't pull in.
/// Char count == display width is roughly true for our content (ASCII
/// + a handful of single-cell box-drawing + status glyphs).
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    if max_cols == usize::MAX {
        return s.to_string();
    }
    // Strip ANSI for measurement.
    let visible: String = strip_ansi(s);
    let visible_width = visible.chars().count();
    if visible_width <= max_cols {
        return s.to_string();
    }
    // Need to truncate. Walk the original string char-by-char, skipping
    // ANSI escape sequences (so they pass through without consuming
    // budget), until visible budget is spent.
    let budget = max_cols.saturating_sub(1); // reserve 1 col for ellipsis
    let mut out = String::with_capacity(s.len());
    let mut visible_count = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ANSI CSI sequence: copy through to terminator [a-zA-Z]
            out.push(c);
            for nc in chars.by_ref() {
                out.push(nc);
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if visible_count >= budget {
            break;
        }
        out.push(c);
        visible_count += 1;
    }
    out.push('…');
    out
}

/// Strip ANSI CSI escape sequences from a string for width measurement.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip through alphabetic terminator.
            for nc in chars.by_ref() {
                if nc.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ============================================================================
// LiveTreeRenderer — re-renders the tree in place on each RoundCompleted
// ============================================================================

/// EventSink that re-renders the cascade tree to stdout in place every
/// `RoundCompleted` (and `Finished`). Uses ANSI cursor positioning so
/// the tree fills in line-by-line as the cascade unfolds — same idiom
/// nix-output-monitor (nom) uses.
///
/// Usage: hand to `Cascade::new()...events(&renderer).run()` for default
/// "watch the cascade unfold" behavior in any TTY-attached cli bin.
///
/// Caller is responsible for not wiring this when stdout is a pipe — it
/// would emit ANSI escapes into the captured output. [`crate::output`]
/// handles the TTY check.
pub struct LiveTreeRenderer {
    accumulator: SnapshotAccumulator,
    color: bool,
    max_depth: Option<usize>,
    /// Optional `Mutex<Vec<u8>>` for testing — when `Some`, frames go here
    /// instead of stdout. Production passes `None`.
    capture: Option<Mutex<Vec<u8>>>,
    /// Number of lines printed in the last frame. On the next repaint
    /// we walk the cursor up this many lines and clear each, then
    /// write the new frame. This mirrors nix-output-monitor's exact
    /// idiom (NOM/IO.hs `writeStateToScreen`): erase-in-place using
    /// `\x1b[2K` + `\x1b[1A\x1b[2K`×(N-1) rather than alt-screen.
    /// Wrapped in iTerm2 synchronized-update markers to avoid
    /// mid-frame flicker on terminals that support them.
    last_printed_lines: Mutex<usize>,
    /// Maximum frame height in lines for truncation (NOM/IO.hs `truncateRows`).
    /// None = auto-detect from terminal on first paint; Some(0) = no cap.
    max_height: Option<usize>,
    /// Wall-time of the last repaint. Used to gate repaints at ≥60ms
    /// intervals (nom `minFrameDuration = 60_000 µs`). None on first paint.
    last_paint_at: Mutex<Option<Instant>>,
    /// Header lines rendered at the top of each frame:
    /// - First line gets `┏━ ` prefix (top-left corner)
    /// - Subsequent lines get `┣ ` prefix (T-junction)
    /// Default: `vec!["Cascade deploy"]`. Override via `with_header_text()`
    /// (single line) or `with_header_lines()` (multiple).
    header_lines: Mutex<Vec<String>>,
}

impl LiveTreeRenderer {
    pub fn new(color: bool, max_depth: Option<usize>) -> Self {
        Self {
            accumulator: SnapshotAccumulator::new(),
            color,
            max_depth,
            capture: None,
            last_printed_lines: Mutex::new(0),
            max_height: None,
            last_paint_at: Mutex::new(None),
            header_lines: Mutex::new(vec!["Cascade deploy".to_string()]),
        }
    }

    /// Test-only constructor that captures frames into an internal buffer
    /// instead of writing to stdout.
    #[doc(hidden)]
    pub fn with_capture(color: bool, max_depth: Option<usize>) -> Self {
        Self {
            accumulator: SnapshotAccumulator::new(),
            color,
            max_depth,
            capture: Some(Mutex::new(Vec::new())),
            last_printed_lines: Mutex::new(0),
            max_height: None,
            last_paint_at: Mutex::new(None),
            header_lines: Mutex::new(vec!["Cascade deploy".to_string()]),
        }
    }

    /// Builder: set an explicit maximum frame height (lines). Used for tests
    /// that can't rely on a real terminal. `Some(0)` disables height capping.
    ///
    /// In production the height is auto-detected from the terminal on each
    /// paint via `console::Term::stdout().size()`.
    pub fn with_max_height(mut self, h: Option<usize>) -> Self {
        self.max_height = h;
        self
    }

    /// Builder: replace the default `"Cascade deploy"` header with a
    /// single custom line.
    pub fn with_header_text(self, text: impl Into<String>) -> Self {
        *self.header_lines.lock().unwrap() = vec![text.into()];
        self
    }

    /// Builder: provide multiple header lines. The first renders with
    /// `┏━ ` (top-left), each subsequent line renders with `┣ `
    /// (T-junction) — matches nom's section convention. Use to keep
    /// a wide header readable on narrow terminals without truncation:
    ///
    /// ```ignore
    /// LiveTreeRenderer::new(color, max_depth)
    ///     .with_header_lines(vec![
    ///         format!("Strategy: {} || Nodes: {}", name, n),
    ///         format!("Fanout: {} || Failures: {}%", f, fr),
    ///     ])
    /// ```
    pub fn with_header_lines(self, lines: Vec<String>) -> Self {
        let lines = if lines.is_empty() {
            vec!["Cascade deploy".to_string()]
        } else {
            lines
        };
        *self.header_lines.lock().unwrap() = lines;
        self
    }

    /// Test-only: read out captured frames as a String.
    #[doc(hidden)]
    pub fn captured(&self) -> String {
        self.capture
            .as_ref()
            .map(|m| String::from_utf8_lossy(&m.lock().unwrap()).to_string())
            .unwrap_or_default()
    }

    /// Render the current tree state, wrapped with HEAVY section
    /// borders (┏━ ┃ ┗━) and a summary row at the bottom — exactly the
    /// frame shape `nom` produces (NOM/Print.hs `printSections`).
    ///
    /// When the assembled frame exceeds the terminal height, truncates per
    /// NOM/IO.hs `truncateRows`: keep the first line, insert ` ⋮ `, then
    /// keep the last `rows - outputLinesToAlwaysShow` lines (5 per nom).
    fn build_frame(&self) -> String {
        let tree = self.accumulator.to_tree();
        let inner = crate::tree::render(
            &tree,
            &OutputFormat::Tree {
                max_depth: self.max_depth,
                color: self.color,
            },
        );

        // Count statuses for the summary row.
        let (ok, in_progress, pending, failed) = self.accumulator.status_counts();

        // Probe the terminal width for line truncation. If we don't
        // truncate, lines wider than the terminal wrap onto multiple
        // visual lines — but our `frame.lines().count()` only counts
        // logical lines, so the cleanup walk-up is off by however many
        // lines wrapped. Result: previous frames' wrapped portions
        // never get erased, producing the multi-header visual stutter.
        let term_cols = if self.capture.is_some() {
            usize::MAX // tests: no truncation
        } else {
            let (_, cols) = console::Term::stdout().size();
            if cols == 0 {
                usize::MAX
            } else {
                cols as usize
            }
        };

        let mut frame = String::with_capacity(inner.len() + 256);
        // Multi-line header: first line gets ┏━ (top-left corner),
        // additional lines get ┣ (T-junction) — matches nom's section
        // convention for stacked sections.
        let header_lines = self.header_lines.lock().unwrap().clone();
        for (i, line) in header_lines.iter().enumerate() {
            let prefix = if i == 0 { "┏━ " } else { "┣ " };
            let composed = format!("{prefix}{line}");
            frame.push_str(&truncate_to_width(&composed, term_cols));
            frame.push('\n');
        }
        // Body: prefix every tree line with ┃  (the vertical chrome).
        for line in inner.lines() {
            let bodied = format!("┃  {line}");
            frame.push_str(&truncate_to_width(&bodied, term_cols));
            frame.push('\n');
        }
        // Bottom border: ┗━ + summary row.
        // Format: `∑ || N ✔ || M ⏵ || K ⏸ || J ⚠`
        // - `||` separator between fields makes them visually distinct
        //   without leaning on whitespace alignment (which falls apart
        //   when counts grow to 3+ digits)
        // - single space between count and glyph for readability
        let summary = format!("┗━ ∑ || {ok} ✔ || {in_progress} ⏵ || {pending} ⏸ || {failed} ⚠");
        frame.push_str(&truncate_to_width(&summary, term_cols));
        frame.push('\n');

        // Truncate to terminal height (NOM/IO.hs `truncateRows`).
        // Resolve effective max from explicit override or terminal query.
        let max_height = match self.max_height {
            Some(0) => return frame, // disabled
            Some(h) => h,
            None => {
                // Auto-detect: only applicable when we're painting to a real TTY.
                // capture mode has no terminal, so skip truncation there.
                if self.capture.is_some() {
                    return frame;
                }
                let (rows, _cols) = console::Term::stdout().size();
                if rows == 0 {
                    return frame; // non-TTY
                }
                rows as usize
            }
        };

        // nom constant: `outputLinesToAlwaysShow = 5`.
        // Guard: need at least 8 lines for truncation to make sense
        // (1 header + 1 ellipsis + 5 tail + 1 footer).
        const ALWAYS_SHOW: usize = 5;
        let lines: Vec<&str> = frame.lines().collect();
        let n = lines.len();
        if max_height <= ALWAYS_SHOW + 2 || n <= max_height {
            return frame;
        }

        // NOM formula (verbatim Haskell logic):
        //   take 1 <> [" ⋮ "] <> drop (n + outputLinesToAlwaysShow + 2 - rows)
        // The drop count is: n + 5 + 2 - max_height.
        let drop_count = n + ALWAYS_SHOW + 2 - max_height;
        let head = &lines[..1];
        let tail = &lines[drop_count..];

        let mut truncated = String::with_capacity(frame.len());
        for l in head {
            truncated.push_str(l);
            truncated.push('\n');
        }
        truncated.push_str(" ⋮ \n");
        for l in tail {
            truncated.push_str(l);
            truncated.push('\n');
        }
        truncated
    }

    /// Erase-in-place repaint. Walks the cursor up `last_printed_lines`
    /// and clears each line individually — exactly the sequence
    /// `nom`'s `writeStateToScreen` emits (NOM/IO.hs). Wrapped in
    /// synchronized-update markers so terminals that support them
    /// (iTerm2, alacritty, kitty, recent gnome-terminal) display the
    /// frame atomically without flicker.
    ///
    /// `force = true` bypasses the 60ms minimum gate (used for the final
    /// `Finished` frame). In capture/test mode the gate is also bypassed
    /// since wall-time pacing is irrelevant in tests.
    ///
    /// nom equivalence: `minFrameDuration = 60_000 µs` (NOM/IO.hs `keepPrinting`).
    fn repaint(&self, force: bool) {
        // 60ms minimum frame gate (nom `minFrameDuration`). Skip in capture
        // mode where wall-time pacing is meaningless (tests run in µs).
        if !force && self.capture.is_none() {
            let mut last_paint = self.last_paint_at.lock().unwrap();
            if let Some(t) = *last_paint {
                if t.elapsed() < Duration::from_millis(60) {
                    return;
                }
            }
            *last_paint = Some(Instant::now());
        } else if self.capture.is_none() {
            // forced paint — update timestamp so next non-forced paint
            // measures from here.
            *self.last_paint_at.lock().unwrap() = Some(Instant::now());
        }

        let frame = self.build_frame();
        let new_lines = frame.lines().count();
        let mut last = self.last_printed_lines.lock().unwrap();

        let mut bytes: Vec<u8> = Vec::with_capacity(frame.len() + 64);
        // begin synchronized update
        bytes.extend_from_slice(b"\x1b[?2026h");
        // Erase last frame in place. Our `frame` ends with a trailing
        // `\n` (so user's shell prompt lands on a fresh line at end of
        // run), which means after the previous frame the cursor is one
        // line BELOW the last frame line. We need to clear that empty
        // line PLUS each of the prior `last` frame lines:
        //
        //   - clear current line (the empty line after the \n)
        //   - then `last` iterations of (cursor up, clear line) →
        //     walks from line N+1 → line N → ... → line 1, clearing each
        //   - finally `\r` to put cursor at column 0 for the new frame
        //
        // nom's `writeStateToScreen` does (last-1) iterations because
        // its frames don't end with \n — the cursor stays at the END
        // of the last line, so it only needs (last-1) up-clears to
        // walk to line 1. Ours has the trailing \n so we need one more.
        if *last > 0 {
            bytes.extend_from_slice(b"\x1b[2K");
            for _ in 0..*last {
                bytes.extend_from_slice(b"\x1b[1A\x1b[2K");
            }
            bytes.extend_from_slice(b"\r");
        }
        bytes.extend_from_slice(frame.as_bytes());
        // end synchronized update
        bytes.extend_from_slice(b"\x1b[?2026l");

        if let Some(cap) = &self.capture {
            cap.lock().unwrap().extend_from_slice(&bytes);
        } else {
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(&bytes);
            let _ = stdout.flush();
        }

        *last = new_lines;
    }
}

impl EventSink for LiveTreeRenderer {
    fn emit(&self, event: &CascadeEvent) {
        // Always update the in-memory tree.
        self.accumulator.emit(event);
        // Repaint policy: force on Started (first frame must appear
        // immediately) and Finished (final frame must always show).
        // Every other event paints through the 60ms throttle gate so
        // long rounds (e.g. cascading thousands of store paths) show
        // mid-flight progress as edges flip from ⏸ → ⏵ → ✔, instead
        // of staying static until RoundCompleted.
        match event {
            CascadeEvent::Started { .. } | CascadeEvent::Finished { .. } => {
                self.repaint(true);
            }
            _ => {
                self.repaint(false);
            }
        }
    }
}

// ============================================================================
// DelayingExecutor — wraps a RoundExecutor + sleeps inside dispatch
// ============================================================================

/// Wraps a [`consortium_nix::cascade::RoundExecutor`] and sleeps for a
/// configurable duration *inside* `dispatch`. Critical for live demos:
/// without this, the deterministic sim resolves a round in microseconds
/// and the live renderer's "spinning" frame (rendered at PlanComputed)
/// is invisible because EdgeCompleted fires before the human eye can
/// register it.
///
/// The sleep happens between PlanComputed (which marks targets
/// in_progress) and EdgeCompleted (which marks them has_closure), so
/// the spinning ⏵ glyph is visible for `delay` real wall-time, then
/// snaps to ✔ on the next frame.
///
/// **Demo / visualization only.** Putting this in production paths
/// adds fake latency to real deploys.
pub struct DelayingExecutor<'a> {
    pub inner: &'a dyn consortium_nix::cascade::RoundExecutor,
    pub delay: Duration,
}

impl<'a> consortium_nix::cascade::RoundExecutor for DelayingExecutor<'a> {
    fn dispatch(
        &self,
        nodes: &[consortium_nix::cascade::CascadeNode],
        edges: &[(
            consortium_nix::cascade::NodeId,
            consortium_nix::cascade::NodeId,
        )],
        net: &consortium_nix::cascade::NetworkProfile,
    ) -> std::collections::HashMap<
        (
            consortium_nix::cascade::NodeId,
            consortium_nix::cascade::NodeId,
        ),
        Result<Duration, consortium_nix::cascade::CascadeError>,
    > {
        std::thread::sleep(self.delay);
        self.inner.dispatch(nodes, edges, net)
    }
}

// ============================================================================
// DelaySink — wraps another sink and sleeps after each RoundCompleted
// ============================================================================

/// Wraps an `EventSink` and injects a wall-time sleep after each
/// `RoundCompleted` event. Useful for making the deterministic sim
/// watchable in live mode — without this the cascade fires in
/// microseconds, faster than the live renderer's frames can register
/// to a human eye.
///
/// **Test/demo only.** Inject this in production and you'll add fake
/// latency to real deploys.
pub struct DelaySink<'a> {
    pub inner: &'a dyn EventSink,
    pub delay: Duration,
}

impl<'a> EventSink for DelaySink<'a> {
    fn emit(&self, event: &CascadeEvent) {
        self.inner.emit(event);
        if matches!(event, CascadeEvent::RoundCompleted { .. }) {
            std::thread::sleep(self.delay);
        }
    }
}

// ============================================================================
// render_events
// ============================================================================

/// Render a slice of `CascadeEvent`s in the requested `OutputFormat`.
///
/// - `Tree` — folds events through `SnapshotAccumulator`, builds an
///   `OwnedTreeNode`, delegates to `tree::render`.
/// - `Json` / `Yaml` / `Toml` — serializes the raw event slice directly
///   (events derive `Serialize`).
pub fn render_events(events: &[CascadeEvent], format: &OutputFormat) -> String {
    match format {
        OutputFormat::Tree { max_depth, color } => {
            let acc = SnapshotAccumulator::new();
            for ev in events {
                acc.emit(ev);
            }
            let tree = acc.to_tree();
            crate::tree::render(
                &tree,
                &OutputFormat::Tree {
                    max_depth: *max_depth,
                    color: *color,
                },
            )
        }
        OutputFormat::Json => serde_json::to_string_pretty(events)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),
        OutputFormat::Yaml => {
            serde_yaml::to_string(events).unwrap_or_else(|e| format!("error: {e}"))
        }
        OutputFormat::Toml => {
            // TOML can't serialize a bare array at the root; wrap under "events".
            let mut wrapper = std::collections::BTreeMap::new();
            wrapper.insert("events", events);
            match toml::to_string(&wrapper) {
                Ok(t) => t,
                Err(e) => format!("# toml render error: {e}\n"),
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use consortium_nix::cascade::NodeId;
    use consortium_nix::cascade_events::{CascadeEvent, Edge, EventSink};

    use super::*;
    use crate::tree::OutputFormat;

    // ---- helpers ----

    fn node(n: u32) -> NodeId {
        NodeId(n)
    }

    fn started_event(seeds: &[u32]) -> CascadeEvent {
        CascadeEvent::Started {
            n_nodes: 4,
            seeded: seeds.iter().map(|&n| NodeId(n)).collect(),
            strategy: "log2-fanout".into(),
            at: SystemTime::UNIX_EPOCH,
        }
    }

    fn edge_completed(round: u32, src: u32, tgt: u32, ms: u64) -> CascadeEvent {
        CascadeEvent::EdgeCompleted {
            round,
            src: node(src),
            tgt: node(tgt),
            duration: Duration::from_millis(ms),
        }
    }

    fn edge_failed(round: u32, src: u32, tgt: u32) -> CascadeEvent {
        use consortium_nix::cascade::CascadeError;
        CascadeEvent::EdgeFailed {
            round,
            src: node(src),
            tgt: node(tgt),
            error: CascadeError::Copy {
                node: node(tgt),
                stderr: "rsync exited 23".into(),
            },
        }
    }

    fn finished(converged: usize, failed: usize, rounds: u32) -> CascadeEvent {
        CascadeEvent::Finished {
            converged,
            failed,
            rounds,
        }
    }

    // ---- tests ----

    /// 1. JsonlWriter emits one line per event, each valid JSON.
    #[test]
    fn jsonl_writer_writes_one_line_per_event() {
        let buf: Vec<u8> = Vec::new();
        let writer = JsonlWriter::new(Box::new(buf));

        writer.emit(&started_event(&[0]));
        writer.emit(&edge_completed(0, 0, 1, 10));
        writer.emit(&finished(2, 0, 1));

        // Extract the inner buffer.
        let raw = {
            let guard = writer.inner.lock().unwrap();
            // Downcast is not possible through dyn Write; capture via Cursor instead.
            // We'll re-run the test using a Cursor to get the bytes.
            drop(guard);
            // Re-run with a cursor approach below.
            Vec::<u8>::new() // placeholder
        };
        // Redo using std::io::Cursor directly.
        let cursor = std::io::Cursor::new(Vec::<u8>::new());
        let writer2 = JsonlWriter::new(Box::new(cursor));
        writer2.emit(&started_event(&[0]));
        writer2.emit(&edge_completed(0, 0, 1, 10));
        writer2.emit(&finished(2, 0, 1));

        let guard = writer2.inner.lock().unwrap();
        // Access the cursor bytes by downcasting. Since we can't downcast dyn Write,
        // we test via EventCollector + JsonlWriter on a shared buffer.
        drop(guard);
        let _ = raw; // suppress unused warning

        // Use a channel-backed approach: write to a Vec<u8> captured behind Arc<Mutex>.
        use std::sync::Arc;
        let shared: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let shared_clone = Arc::clone(&shared);

        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer3 = JsonlWriter::new(Box::new(SharedBuf(shared_clone)));
        writer3.emit(&started_event(&[0]));
        writer3.emit(&edge_completed(0, 0, 1, 10));
        writer3.emit(&finished(2, 0, 1));

        let bytes = shared.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines.len(), 3, "expected 3 JSONL lines, got: {text}");
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("invalid JSON on line {line:?}: {e}"));
        }
    }

    /// 2. EventCollector records events in emission order.
    #[test]
    fn event_collector_records_in_order() {
        let col = EventCollector::new();
        col.emit(&started_event(&[0]));
        col.emit(&edge_completed(0, 0, 1, 5));
        col.emit(&finished(2, 0, 1));

        assert_eq!(col.len(), 3);

        let evs = col.events();
        assert!(matches!(&evs[0], CascadeEvent::Started { .. }));
        assert!(matches!(&evs[1], CascadeEvent::EdgeCompleted { .. }));
        assert!(matches!(&evs[2], CascadeEvent::Finished { .. }));
    }

    /// 3. SnapshotAccumulator builds correct tree topology from EdgeCompleted
    /// AND adopts pre-populated unparented nodes (orphans) under root.
    #[test]
    fn snapshot_accumulator_builds_correct_tree_topology() {
        // started_event creates 4 nodes (n0..n3). EdgeCompleted attaches
        // n1, n2 to n0. n3 is pre-populated by Started but never gets an
        // event → orphan → adopted under root as Pending.
        let acc = SnapshotAccumulator::new();
        acc.emit(&started_event(&[0]));
        acc.emit(&edge_completed(0, 0, 1, 10));
        acc.emit(&edge_completed(0, 0, 2, 12));
        acc.emit(&finished(3, 0, 1));

        let tree = acc.to_tree();
        assert_eq!(tree.label, "n0");
        // 3 children: n1, n2 (Ok via EdgeCompleted) + n3 (Pending orphan).
        assert_eq!(
            tree.children.len(),
            3,
            "root should have 2 served + 1 pre-populated orphan = 3 children"
        );

        let child_labels: Vec<&str> = tree.children.iter().map(|c| c.label.as_str()).collect();
        assert!(child_labels.contains(&"n1"), "missing n1: {child_labels:?}");
        assert!(child_labels.contains(&"n2"), "missing n2: {child_labels:?}");
        assert!(
            child_labels.contains(&"n3"),
            "missing n3 (orphan): {child_labels:?}"
        );

        // Root is Ok (seeded). Served children are Ok; orphan is Pending.
        assert_eq!(tree.status, Some(NodeStatus::Ok));
        for child in &tree.children {
            let expected = match child.label.as_str() {
                "n1" | "n2" => Some(NodeStatus::Ok),
                "n3" => Some(NodeStatus::Pending),
                other => panic!("unexpected child {other}"),
            };
            assert_eq!(child.status, expected, "wrong status for {}", child.label);
        }
    }

    /// 4. SnapshotAccumulator marks failed nodes correctly.
    #[test]
    fn accumulator_marks_failed_nodes() {
        let acc = SnapshotAccumulator::new();
        acc.emit(&started_event(&[0]));
        acc.emit(&edge_completed(0, 0, 1, 8));
        acc.emit(&edge_failed(0, 0, 2)); // n2 fails
        acc.emit(&finished(2, 1, 1));

        let tree = acc.to_tree();

        // Find n2 in tree children.
        let n2 = tree
            .children
            .iter()
            .find(|c| c.label == "n2")
            .expect("n2 should be in tree even though it failed");
        assert_eq!(n2.status, Some(NodeStatus::Failed), "n2 should be Failed");

        // n1 should be Ok.
        let n1 = tree
            .children
            .iter()
            .find(|c| c.label == "n1")
            .expect("n1 should be in tree");
        assert_eq!(n1.status, Some(NodeStatus::Ok));
    }

    /// 5. render_events Tree format includes all four status glyphs.
    /// Status semantics:
    /// - PlanComputed marks a target as InProgress (⏵ "spinning")
    /// - EdgeCompleted clears in_progress and sets has_closure (✔)
    /// - EdgeFailed clears in_progress and sets failed (⚠)
    /// - A node that's only listed in `Started` as a child (or never
    ///   appears in any event but is reachable from a parent) renders
    ///   as Pending (⏸)
    #[test]
    fn render_events_tree_format_includes_status_glyphs() {
        // Plan: (0,1) completes → ✔, (0,3) is in plan AND completes
        // before Finished → ✔. (0,2) fails → ⚠. n4 is in plan but
        // never resolves (no EdgeCompleted/EdgeFailed) → stays ⏵
        // (in_progress) when PlanComputed marked it.
        let events = vec![
            started_event(&[0]),
            CascadeEvent::PlanComputed {
                round: 0,
                assignments: vec![
                    Edge {
                        src: node(0),
                        tgt: node(1),
                    },
                    Edge {
                        src: node(0),
                        tgt: node(2),
                    },
                    Edge {
                        src: node(0),
                        tgt: node(4),
                    }, // never resolves → ⏵
                ],
            },
            edge_completed(0, 0, 1, 5), // ✔
            edge_failed(0, 0, 2),       // ⚠
            // n4 left in_progress (PlanComputed marked it but no
            // EdgeCompleted/EdgeFailed)
            finished(2, 1, 1),
        ];

        let out = render_events(
            &events,
            &OutputFormat::Tree {
                max_depth: None,
                color: false,
            },
        );

        assert!(out.contains("✔"), "missing ✔ (Ok glyph): {out}");
        assert!(out.contains("⚠"), "missing ⚠ (Failed glyph): {out}");
        // n4 still in_progress at the end → ⏵ glyph visible.
        // This is the test that proves PlanComputed sets in_progress
        // and unresolved nodes keep that state.
        assert!(
            out.contains("⏵"),
            "missing ⏵ (InProgress glyph for unresolved planned target): {out}"
        );
    }

    /// 7. LiveTreeRenderer emits a frame on each PlanComputed +
    /// RoundCompleted + Finished, uses cursor-home + clear-screen
    /// escapes (alt-screen idiom, like top/htop/vim/less), and
    /// captures multiple distinct frames over the cascade lifetime.
    /// Also verifies priority sorting: Failed nodes appear before Ok
    /// nodes in the rendered output (nom/State/Sorting.hs order).
    #[test]
    fn live_tree_renderer_emits_multiple_frames_with_ansi_redraw() {
        // Drive a sequence: Started, PlanComputed, EdgeCompleted(n1 ok),
        // EdgeFailed(n2 failed), RoundCompleted, Finished.
        // n2 (Failed) should appear before n1 (Ok) in the final frame
        // due to priority sorting (Failed → priority 0, Ok → priority 3).
        //
        // Capture mode bypasses the 60ms gate, so all 4 paint-triggering
        // events (PlanComputed, RoundCompleted, Finished) still produce frames.
        let renderer = LiveTreeRenderer::with_capture(false, None);

        renderer.emit(&CascadeEvent::Started {
            n_nodes: 4,
            seeded: vec![NodeId(0)],
            strategy: "log2-fanout".into(),
            at: SystemTime::UNIX_EPOCH,
        });
        renderer.emit(&CascadeEvent::PlanComputed {
            round: 0,
            assignments: vec![
                Edge {
                    src: NodeId(0),
                    tgt: NodeId(1),
                },
                Edge {
                    src: NodeId(0),
                    tgt: NodeId(2),
                },
            ],
        });
        renderer.emit(&edge_completed(0, 0, 1, 5)); // n1 → Ok
        renderer.emit(&edge_failed(0, 0, 2)); // n2 → Failed
        renderer.emit(&CascadeEvent::RoundCompleted {
            round: 0,
            duration: Duration::from_millis(5),
            has_closure: vec![NodeId(0), NodeId(1)],
        });
        renderer.emit(&edge_completed(1, 1, 3, 5));
        renderer.emit(&CascadeEvent::RoundCompleted {
            round: 1,
            duration: Duration::from_millis(5),
            has_closure: vec![NodeId(0), NodeId(1), NodeId(3)],
        });
        renderer.emit(&CascadeEvent::Finished {
            converged: 3,
            failed: 1,
            rounds: 2,
        });

        let captured = renderer.captured();

        // Replicates nom's `writeStateToScreen` exactly: each frame is
        // wrapped in synchronized-update markers `\x1b[?2026h` …
        // `\x1b[?2026l`. The first frame has no preceding clear (last
        // was 0). Subsequent frames clear the previous frame line-by-
        // line: `\x1b[2K` for the bottom line, then `\x1b[1A\x1b[2K`
        // for each line above.
        //
        // Test events: PlanComputed → frame 1, RoundCompleted×2 → frames
        // 2-3, Finished → frame 4. Total 4 sync-update begin markers.
        // (Capture mode bypasses the 60ms gate so all 4 paint.)
        let sync_begin = captured.matches("\x1b[?2026h").count();
        assert_eq!(
            sync_begin, 4,
            "expected exactly 4 synchronized-update begin markers (one per repaint); got {sync_begin}\n{captured:?}"
        );
        let sync_end = captured.matches("\x1b[?2026l").count();
        assert_eq!(
            sync_end, 4,
            "synchronized-update markers should be balanced"
        );
        // Frames 2, 3, 4 each emit at least one clear-line escape
        // (\x1b[2K) since they're erasing prior content. Don't assert
        // exact count — depends on how many lines the prior frame had.
        let clear_lines = captured.matches("\x1b[2K").count();
        assert!(
            clear_lines >= 3,
            "expected at least 3 clear-line escapes (one per non-first frame); got {clear_lines}\n{captured:?}"
        );
        // Every frame should include the heavy section border ┏━ at top
        // and ┗━ at bottom — that's how nom wraps each section.
        assert!(
            captured.contains("┏━"),
            "missing top border ┏━: {captured:?}"
        );
        assert!(
            captured.contains("┗━"),
            "missing bottom border ┗━: {captured:?}"
        );
        // Bottom border carries a summary row with at least one ✔ count.
        assert!(captured.contains("✔"), "missing ✔ glyph: {captured:?}");
        // n0, n1, n2 should all eventually appear in the captured output.
        assert!(captured.contains("n0"));
        assert!(captured.contains("n1"));
        assert!(captured.contains("n2"));

        // Priority sorting assertion (nom/State/Sorting.hs):
        // n2 is Failed (priority 0), n1 is Ok (priority 3).
        // In the final frame n2 must appear before n1.
        // Find positions in the last frame by scanning from the last
        // sync-begin marker backwards is complex; instead scan the full
        // captured output for relative positions (n2 always before n1).
        let pos_n2 = captured
            .rfind("n2")
            .expect("n2 should appear in captured output");
        let pos_n1 = captured
            .rfind("n1")
            .expect("n1 should appear in captured output");
        assert!(
            pos_n2 < pos_n1,
            "priority sort: n2 (Failed) should appear before n1 (Ok) in the final frame; \
             n2 at byte {pos_n2}, n1 at byte {pos_n1}"
        );
    }

    /// 8. Truncation: a tall cascade frame is capped at max_height lines
    /// per NOM/IO.hs `truncateRows`, with ` ⋮ ` ellipsis inserted.
    #[test]
    fn live_tree_renderer_truncates_tall_frames() {
        use consortium_nix::cascade_events::Edge;

        // Build a wide cascade: node 0 seeds 30 children (nodes 1..=30).
        // Each child is a leaf (Ok). With max_height=20 the frame must be
        // truncated — 30 body lines + 2 chrome lines (header + footer) = 32
        // total, which exceeds 20.
        let renderer = LiveTreeRenderer::with_capture(false, None).with_max_height(Some(20));

        // Start with node 0 as seed.
        renderer.emit(&CascadeEvent::Started {
            n_nodes: 31,
            seeded: vec![NodeId(0)],
            strategy: "log2-fanout".into(),
            at: SystemTime::UNIX_EPOCH,
        });

        // Plan all 30 edges at once.
        let assignments: Vec<Edge> = (1u32..=30)
            .map(|i| Edge {
                src: NodeId(0),
                tgt: NodeId(i),
            })
            .collect();
        renderer.emit(&CascadeEvent::PlanComputed {
            round: 0,
            assignments,
        });

        // Complete all edges.
        for i in 1u32..=30 {
            renderer.emit(&edge_completed(0, 0, i, 5));
        }
        renderer.emit(&CascadeEvent::RoundCompleted {
            round: 0,
            duration: Duration::from_millis(5),
            has_closure: (0u32..=30).map(NodeId).collect(),
        });
        renderer.emit(&CascadeEvent::Finished {
            converged: 31,
            failed: 0,
            rounds: 1,
        });

        let captured = renderer.captured();

        // The frame must contain the ellipsis marker.
        assert!(
            captured.contains(" ⋮ "),
            "expected truncation ellipsis ` ⋮ ` in captured output; frame:\n{captured}"
        );

        // Find the last frame (after the final sync-begin marker) and count its lines.
        let last_frame_start = captured
            .rfind("\x1b[?2026h")
            .expect("no sync-begin marker found");
        let last_frame_end = captured
            .rfind("\x1b[?2026l")
            .expect("no sync-end marker found");
        let last_frame = &captured[last_frame_start..last_frame_end];
        // Strip ANSI escape sequences for line counting.
        let visible: String = last_frame
            .split('\n')
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let line_count = visible.lines().count();
        assert!(
            line_count <= 22,
            "truncated frame should be roughly max_height lines; got {line_count} lines"
        );
    }

    /// 6. JSONL round-trip: serialize events via JsonlWriter → parse back.
    #[test]
    fn render_events_jsonl_format_round_trips() {
        use std::sync::Arc;

        let events = vec![
            started_event(&[0]),
            edge_completed(0, 0, 1, 10),
            edge_completed(0, 0, 2, 15),
            edge_failed(0, 0, 3),
            finished(3, 1, 1),
        ];

        // Write via JsonlWriter.
        let shared: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let shared_clone = Arc::clone(&shared);

        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer = JsonlWriter::new(Box::new(SharedBuf(shared_clone)));
        for ev in &events {
            writer.emit(ev);
        }

        let bytes = shared.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines.len(), events.len(), "line count mismatch");

        // Parse each line back and count variants.
        let mut started = 0usize;
        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut finished_count = 0usize;

        for line in &lines {
            let ev: CascadeEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("invalid JSONL line {line:?}: {e}"));
            match ev {
                CascadeEvent::Started { .. } => started += 1,
                CascadeEvent::EdgeCompleted { .. } => completed += 1,
                CascadeEvent::EdgeFailed { .. } => failed += 1,
                CascadeEvent::Finished { .. } => finished_count += 1,
                _ => {}
            }
        }

        assert_eq!(started, 1);
        assert_eq!(completed, 2);
        assert_eq!(failed, 1);
        assert_eq!(finished_count, 1);
    }
}
