//! Per-run result report in the schema consumed by CON-20 ("Observability
//! & structured test-report pipeline").
//!
//! [`CascadeTrace::to_json`](consortium_nix::cascade_trace::CascadeTrace::to_json)
//! emits the *raw* per-round trace: plan, outcomes, parent chain. This
//! module derives the *metrics* the report pipeline aggregates across
//! runs:
//!
//! | metric | source |
//! |---|---|
//! | fan-out depth | final `parent_chain` |
//! | time-to-converge | cumulative round durations |
//! | per-edge throughput | successful edge outcomes x closure size |
//! | failure subtrees | [`CascadeError`] tree, shape preserved |
//! | per-phase timings | [`DagSimReport`] stage timings |
//!
//! Both exporters are deterministic for a given seed: every collection
//! is sorted by node id before emission, so two runs of the same scenario
//! produce byte-identical JSON. That is what makes the report usable as
//! a regression baseline.
//!
//! # Example
//!
//! ```
//! use consortium_fanout_sim::report::RunReport;
//! use consortium_fanout_sim::scenario::{Scenario, ScenarioConfig};
//! use consortium_nix::cascade::{CascadeStrategy, Log2FanOut};
//! use consortium_nix::cascade_trace::TraceRecorder;
//!
//! let cfg = ScenarioConfig { seed: 7, n_nodes: 32, ..ScenarioConfig::default() };
//! let rec = TraceRecorder::new();
//! let (result, trace) = Scenario::new(cfg.clone()).run_traced(&Log2FanOut, &rec);
//!
//! let report = RunReport::from_cascade(
//!     RunReport::meta(&cfg, Log2FanOut.name()),
//!     &result,
//!     &trace,
//!     cfg.closure_bytes,
//! );
//! assert_eq!(report.fan_out_depth, 5);
//! assert!(report.is_success());
//! let json = report.to_json();
//! assert!(json.contains("\"schema_version\": 1"));
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use consortium_nix::cascade::{CascadeError, CascadeResult, NodeId};

use crate::dag_sim::DagSimReport;

// ============================================================================
// Schema metadata
// ============================================================================

/// Version of the emitted JSON schema. Bump on any breaking field change
/// so a baseline recorded by an older run is rejected rather than
/// silently misread.
pub const SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Metric records
// ============================================================================

/// Throughput of one successful cascade edge.
///
/// `goodput` is bytes actually delivered per wall-second. On a lossy link
/// the wire moved more than `bytes` (retransmits), so goodput is strictly
/// below the link's nominal bandwidth — that gap is the signal worth
/// graphing.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeThroughput {
    pub src: NodeId,
    pub tgt: NodeId,
    /// Round in which this edge fired.
    pub round: u32,
    /// Round depth of the target once this edge landed.
    pub depth: u32,
    /// Closure bytes delivered over this edge.
    pub bytes: u64,
    /// Wall-time charged to this edge, including any retransmits.
    pub duration: Duration,
}

impl EdgeThroughput {
    /// Delivered bytes per second. `0.0` for a zero-duration edge.
    pub fn goodput_bytes_per_sec(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs <= 0.0 {
            0.0
        } else {
            self.bytes as f64 / secs
        }
    }
}

/// One node's position in the cascade tree, and when it converged.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeConvergence {
    pub node: NodeId,
    /// 0 for a pre-seeded node, else `1 + depth(parent)`.
    pub depth: u32,
    /// Round in which the node first held the closure. `0` if pre-seeded.
    pub round: u32,
    /// Cumulative wall-time at the end of that round.
    pub converged_after: Duration,
}

/// A failed subtree, mirroring the shape of the [`CascadeError`] tree.
///
/// Preserved rather than flattened so the report can answer "which hosts
/// went down because of *this* node" — the parent-path bubbling that
/// `cascade.rs` does is the whole point.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureSubtree {
    /// The node whose subtree failed. For a leaf error this is the node
    /// that actually failed; for an aggregate it is the branch point.
    pub node: NodeId,
    /// `Copy` / `SshHandshake` / `Activation` / `Partitioned` / `SubtreeAggregate`.
    pub kind: &'static str,
    /// Human-readable detail, e.g. the `stderr` of a failed copy.
    pub detail: String,
    /// Whether the coordinator may retry this from a different source.
    pub transient: bool,
    pub children: Vec<FailureSubtree>,
}

impl FailureSubtree {
    /// Flatten to `(node, kind)` pairs, depth-first in tree order.
    pub fn leaves(&self) -> Vec<(NodeId, &'static str)> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<(NodeId, &'static str)>) {
        if self.children.is_empty() {
            out.push((self.node, self.kind));
        } else {
            for c in &self.children {
                c.collect_leaves(out);
            }
        }
    }

    /// Build from a cascade error tree.
    pub fn from_error(err: &CascadeError) -> Self {
        match err {
            CascadeError::Copy { node, stderr } => Self {
                node: *node,
                kind: "Copy",
                detail: stderr.clone(),
                transient: err.is_transient(),
                children: Vec::new(),
            },
            CascadeError::SshHandshake { node, parent } => Self {
                node: *node,
                kind: "SshHandshake",
                detail: format!("unreachable from parent {}", parent.0),
                transient: false,
                children: Vec::new(),
            },
            CascadeError::Activation { node, stage } => Self {
                node: *node,
                kind: "Activation",
                detail: format!("activation failed at stage {stage}"),
                transient: false,
                children: Vec::new(),
            },
            CascadeError::Partitioned { src, tgt } => Self {
                node: *tgt,
                kind: "Partitioned",
                detail: format!("partitioned from {}", src.0),
                transient: false,
                children: Vec::new(),
            },
            CascadeError::SubtreeAggregate { node, errors } => Self {
                node: *node,
                kind: "SubtreeAggregate",
                detail: format!("{} failure(s) below", errors.len()),
                transient: false,
                children: errors.iter().map(FailureSubtree::from_error).collect(),
            },
        }
    }
}

/// Timing for one phase of the nix-deploy DAG.
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseTiming {
    /// `eval` / `health` / `build` / `copy` / `activate`.
    pub phase: String,
    pub limit: usize,
    pub makespan: Duration,
    pub total_work: Duration,
    pub peak_concurrency: usize,
    pub converged_hosts: usize,
    /// `total_work / (makespan * peak_concurrency)`, in `(0, 1]`.
    pub parallel_efficiency: f64,
}

impl PhaseTiming {
    fn from_stage(s: &crate::dag_sim::StageTiming) -> Self {
        Self {
            phase: s.stage.name().to_string(),
            limit: s.limit,
            makespan: s.makespan,
            total_work: s.total_work,
            peak_concurrency: s.peak_concurrency,
            converged_hosts: s.converged_hosts,
            parallel_efficiency: s.parallel_efficiency,
        }
    }
}

// ============================================================================
// Run metadata
// ============================================================================

/// Scenario identity, so a report is self-describing in an artifact dir.
#[derive(Debug, Clone, PartialEq)]
pub struct RunMeta {
    /// The seed that produced this run. Two reports with equal seeds must
    /// be byte-identical.
    pub seed: u64,
    pub n_nodes: u32,
    pub strategy: String,
    pub closure_bytes: u64,
    /// Hosts holding the closure before the cascade started.
    pub seeded: u32,
}

// ============================================================================
// RunReport
// ============================================================================

/// One simulated run, in the CON-20 report schema.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub schema_version: u32,
    pub run: RunMeta,
    /// Deepest node in the cascade tree, counting the seed as 0.
    ///
    /// This is the fan-out width the strategy actually achieved — a
    /// `log2`-fanout over 1024 hosts reports 10.
    pub fan_out_depth: u32,
    /// Nodes per level of the deepest round, i.e. the widest ring.
    pub fan_out_width: u32,
    pub rounds: u32,
    /// Cumulative wall-time until the last node held the closure.
    /// `None` if the run failed before full convergence.
    pub time_to_converge: Option<Duration>,
    /// Per-host convergence, sorted by node id.
    pub convergence: Vec<NodeConvergence>,
    /// Every successful edge, sorted by `(src, tgt)`.
    pub edge_throughput: Vec<EdgeThroughput>,
    /// `None` when the run fully succeeded.
    pub failure_subtree: Option<FailureSubtree>,
    /// Deploy-DAG phase timings; empty for a cascade-only run.
    pub phase_timings: Vec<PhaseTiming>,
    /// Makespan of the simulated deploy, when phase timings are present.
    pub deploy_wall_time: Option<Duration>,
}

impl RunReport {
    /// Build scenario metadata. Kept separate so the same descriptor can
    /// be reused across strategies.
    pub fn meta(cfg: &crate::scenario::ScenarioConfig, strategy: impl Into<String>) -> RunMeta {
        RunMeta {
            seed: cfg.seed,
            n_nodes: cfg.n_nodes,
            strategy: strategy.into(),
            closure_bytes: cfg.closure_bytes,
            seeded: if cfg.seed_fraction > 0.0 {
                (cfg.n_nodes as f64 * cfg.seed_fraction).round() as u32
            } else {
                1
            },
        }
    }

    /// Derive every cascade-side metric from a completed run.
    ///
    /// `trace` supplies the per-edge outcomes and the parent chain;
    /// `closure_bytes` prices each edge. Pass the same seed/closure you
    /// gave the executor or the throughput numbers will be fiction.
    pub fn from_cascade(
        run: RunMeta,
        result: &CascadeResult,
        trace: &consortium_nix::cascade_trace::CascadeTrace,
        closure_bytes: u64,
    ) -> Self {
        // -- parent chain: the last snapshot has the complete tree --
        let parents: BTreeMap<u32, u32> = trace
            .snapshots
            .last()
            .map(|s| s.parent_chain.iter().map(|(n, p)| (n.0, p.0)).collect())
            .unwrap_or_default();

        // -- depth per node, memoized over the parent chain --
        let mut depth: BTreeMap<u32, u32> = BTreeMap::new();
        for &node in &result.converged {
            depth_of(node.0, &parents, &mut depth);
        }

        let fan_out_depth = depth.values().copied().max().unwrap_or(0);
        let fan_out_width = depth.values().filter(|d| **d == fan_out_depth).count() as u32;

        // -- per-round convergence time --
        // cumulative wall-time at the end of each round
        let mut elapsed: Vec<Duration> = Vec::with_capacity(trace.snapshots.len());
        let mut acc = Duration::ZERO;
        for snap in &trace.snapshots {
            acc += snap.round_duration;
            elapsed.push(acc);
        }

        // -- which round did each node converge in? --
        // first snapshot whose has_closure_after contains the node
        let mut round_of: BTreeMap<u32, u32> = BTreeMap::new();
        for snap in &trace.snapshots {
            for n in &snap.has_closure_after {
                round_of.entry(n.0).or_insert(snap.round);
            }
        }

        let convergence: Vec<NodeConvergence> = result
            .converged
            .iter()
            .map(|n| {
                let round = round_of.get(&n.0).copied().unwrap_or(0);
                NodeConvergence {
                    node: *n,
                    depth: depth.get(&n.0).copied().unwrap_or(0),
                    round,
                    converged_after: elapsed
                        .get(round as usize)
                        .copied()
                        .unwrap_or(Duration::ZERO),
                }
            })
            .collect();

        // -- per-edge throughput, from every successful outcome --
        let mut edges: Vec<EdgeThroughput> = Vec::new();
        for snap in &trace.snapshots {
            for ((src, tgt), outcome) in &snap.outcomes {
                if let Ok(dur) = outcome {
                    edges.push(EdgeThroughput {
                        src: *src,
                        tgt: *tgt,
                        round: snap.round,
                        depth: depth.get(&tgt.0).copied().unwrap_or(0),
                        bytes: closure_bytes,
                        duration: *dur,
                    });
                }
            }
        }
        edges.sort_by_key(|e| (e.src.0, e.tgt.0));

        // -- time to converge: the last node's arrival, if the run did --
        let time_to_converge = if result.is_success() {
            elapsed.last().copied()
        } else {
            None
        };

        Self {
            schema_version: SCHEMA_VERSION,
            run,
            fan_out_depth,
            fan_out_width,
            rounds: result.rounds,
            time_to_converge,
            convergence,
            edge_throughput: edges,
            failure_subtree: result.failed.as_ref().map(FailureSubtree::from_error),
            phase_timings: Vec::new(),
            deploy_wall_time: None,
        }
    }

    /// Attach deploy-DAG phase timings from a [`DagSimReport`].
    ///
    /// A cascade-only run leaves these empty; a run that drove both the
    /// cascade and the deploy DAG fills all five phases.
    pub fn with_dag(mut self, dag: &DagSimReport) -> Self {
        self.phase_timings = dag.stages.iter().map(PhaseTiming::from_stage).collect();
        self.deploy_wall_time = Some(dag.wall_time);
        self
    }

    /// Every host received the closure.
    pub fn is_success(&self) -> bool {
        self.failure_subtree.is_none()
    }

    /// Hosts that converged, as reported by the cascade.
    pub fn converged_hosts(&self) -> usize {
        self.convergence.len()
    }

    /// Hosts that never converged.
    pub fn stranded_hosts(&self) -> usize {
        self.run.n_nodes as usize - self.convergence.len()
    }

    /// Goodput of the slowest edge — the critical-path edge in a
    /// bandwidth-bound run.
    pub fn min_goodput(&self) -> Option<f64> {
        self.edge_throughput
            .iter()
            .map(EdgeThroughput::goodput_bytes_per_sec)
            .fold(None, |acc: Option<f64>, g| {
                Some(acc.map_or(g, |a| a.min(g)))
            })
    }

    /// Arithmetic-mean goodput across every successful edge.
    pub fn mean_goodput(&self) -> Option<f64> {
        if self.edge_throughput.is_empty() {
            return None;
        }
        let sum: f64 = self
            .edge_throughput
            .iter()
            .map(EdgeThroughput::goodput_bytes_per_sec)
            .sum();
        Some(sum / self.edge_throughput.len() as f64)
    }

    // -----------------------------------------------------------------------
    // Exporters
    // -----------------------------------------------------------------------

    /// Serialize to the CON-20 per-run JSON schema.
    ///
    /// Deterministic: node-keyed collections are emitted in node-id order,
    /// so repeated runs of the same seed are byte-identical and diff
    /// cleanly against a stored baseline.
    pub fn to_json(&self) -> String {
        use serde_json::{json, Value};

        let mib = 1024.0 * 1024.0;
        let f3 = |v: f64| (v * 1000.0).round() / 1000.0;

        let convergence: Vec<Value> = self
            .convergence
            .iter()
            .map(|c| {
                json!({
                    "node": c.node.0,
                    "depth": c.depth,
                    "round": c.round,
                    "converged_after_ms": c.converged_after.as_secs_f64() * 1000.0,
                })
            })
            .collect();

        let edges: Vec<Value> = self
            .edge_throughput
            .iter()
            .map(|e| {
                json!({
                    "src": e.src.0,
                    "tgt": e.tgt.0,
                    "round": e.round,
                    "depth": e.depth,
                    "bytes": e.bytes,
                    "duration_ms": e.duration.as_secs_f64() * 1000.0,
                    "goodput_mib_per_sec": f3(e.goodput_bytes_per_sec() / mib),
                })
            })
            .collect();

        let failure = match &self.failure_subtree {
            None => Value::Null,
            Some(f) => subtree_json(f),
        };

        let phases: Vec<Value> = self
            .phase_timings
            .iter()
            .map(|p| {
                json!({
                    "phase": p.phase,
                    "limit": p.limit,
                    "makespan_ms": p.makespan.as_secs_f64() * 1000.0,
                    "total_work_ms": p.total_work.as_secs_f64() * 1000.0,
                    "peak_concurrency": p.peak_concurrency,
                    "converged_hosts": p.converged_hosts,
                    "parallel_efficiency": f3(p.parallel_efficiency),
                })
            })
            .collect();

        let root = json!({
            "schema_version": self.schema_version,
            "run": {
                "seed": self.run.seed,
                "n_nodes": self.run.n_nodes,
                "strategy": self.run.strategy,
                "closure_bytes": self.run.closure_bytes,
                "seeded": self.run.seeded,
            },
            "fan_out": {
                "depth": self.fan_out_depth,
                "width": self.fan_out_width,
            },
            "rounds": self.rounds,
            "time_to_converge_ms": self
                .time_to_converge
                .map(|d| d.as_secs_f64() * 1000.0),
            "hosts": {
                "converged": self.converged_hosts(),
                "stranded": self.stranded_hosts(),
            },
            "convergence": convergence,
            "throughput": {
                "edges": edges,
                "min_goodput_mib_per_sec": self.min_goodput().map(|g| f3(g / mib)),
                "mean_goodput_mib_per_sec": self.mean_goodput().map(|g| f3(g / mib)),
            },
            "failure_subtree": failure,
            "phase_timings": phases,
            "deploy_wall_time_ms": self.deploy_wall_time.map(|d| d.as_secs_f64() * 1000.0),
        });

        serde_json::to_string_pretty(&root)
            .expect("serde_json serialization is infallible for Value")
    }

    /// Human-readable Markdown summary, for attaching next to the JSON.
    pub fn to_markdown(&self) -> String {
        let mib = 1024.0 * 1024.0;
        let mut s = String::new();

        s.push_str(&format!("# Fan-out run — seed `{}`\n\n", self.run.seed));
        s.push_str(&format!(
            "- **Fleet**: {} hosts, {} pre-seeded, {} MiB closure\n",
            self.run.n_nodes,
            self.run.seeded,
            self.run.closure_bytes as f64 / mib
        ));
        s.push_str(&format!("- **Strategy**: {}\n", self.run.strategy));
        s.push_str(&format!("- **Rounds**: {}\n", self.rounds));
        s.push_str(&format!(
            "- **Fan-out**: depth {}, width {}\n",
            self.fan_out_depth, self.fan_out_width
        ));
        s.push_str(&format!(
            "- **Convergence**: {} converged, {} stranded\n",
            self.converged_hosts(),
            self.stranded_hosts()
        ));
        match self.time_to_converge {
            Some(d) => s.push_str(&format!(
                "- **Time to converge**: {:.3} s\n",
                d.as_secs_f64()
            )),
            None => s.push_str("- **Time to converge**: did not converge\n"),
        }
        if let Some(g) = self.mean_goodput() {
            s.push_str(&format!("- **Mean goodput**: {:.2} MiB/s\n", g / mib));
        }
        if let Some(g) = self.min_goodput() {
            s.push_str(&format!("- **Min goodput**: {:.2} MiB/s\n", g / mib));
        }

        if !self.phase_timings.is_empty() {
            s.push_str("\n## Deploy phases\n\n");
            s.push_str("| phase | limit | makespan (s) | work (s) | peak | eff | converged |\n");
            s.push_str("|---|---|---|---|---|---|---|\n");
            for p in &self.phase_timings {
                s.push_str(&format!(
                    "| {} | {} | {:.3} | {:.3} | {} | {:.1}% | {} |\n",
                    p.phase,
                    p.limit,
                    p.makespan.as_secs_f64(),
                    p.total_work.as_secs_f64(),
                    p.peak_concurrency,
                    p.parallel_efficiency * 100.0,
                    p.converged_hosts
                ));
            }
        }

        if let Some(f) = &self.failure_subtree {
            s.push_str("\n## Failure subtree\n\n```\n");
            s.push_str(&subtree_ascii(f, 0));
            s.push_str("```\n");
        }

        s
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Depth of `node` in the cascade tree, memoized in `memo`.
///
/// Roots (pre-seeded nodes, absent from `parents`) are depth 0. A node
/// whose parent is itself missing from `parents` is treated as a root
/// rather than recursing forever.
fn depth_of(node: u32, parents: &BTreeMap<u32, u32>, memo: &mut BTreeMap<u32, u32>) -> u32 {
    if let Some(d) = memo.get(&node) {
        return *d;
    }
    let d = match parents.get(&node) {
        Some(&p) if p != node => 1 + depth_of(p, parents, memo),
        _ => 0,
    };
    memo.insert(node, d);
    d
}

fn subtree_json(s: &FailureSubtree) -> serde_json::Value {
    use serde_json::{json, Value};
    let children: Vec<Value> = s.children.iter().map(subtree_json).collect();
    json!({
        "node": s.node.0,
        "kind": s.kind,
        "detail": s.detail,
        "transient": s.transient,
        "children": children,
    })
}

fn subtree_ascii(s: &FailureSubtree, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    let mut out = format!("{pad}{} [{}] {}\n", s.node.0, s.kind, s.detail);
    for c in &s.children {
        out.push_str(&subtree_ascii(c, indent + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::{Scenario, ScenarioConfig};
    use consortium_nix::cascade::{CascadeStrategy, Log2FanOut};
    use consortium_nix::cascade_trace::TraceRecorder;

    fn report_for(seed: u64, n_nodes: u32) -> (RunReport, ScenarioConfig) {
        let cfg = ScenarioConfig {
            seed,
            n_nodes,
            ..ScenarioConfig::default()
        };
        let rec = TraceRecorder::new();
        let (result, trace) = Scenario::new(cfg.clone()).run_traced(&Log2FanOut, &rec);
        let report = RunReport::from_cascade(
            RunReport::meta(&cfg, Log2FanOut.name()),
            &result,
            &trace,
            cfg.closure_bytes,
        );
        (report, cfg)
    }

    #[test]
    fn log2_fanout_depth_is_log2_of_n() {
        for n in [16u32, 64, 256] {
            let (r, _) = report_for(1, n);
            assert_eq!(r.fan_out_depth, n.ilog2(), "n={n}");
            assert_eq!(r.fan_out_width, 1, "log2 fanout has one node per level");
        }
    }

    #[test]
    fn json_is_byte_identical_across_runs_of_the_same_seed() {
        let (a, _) = report_for(42, 64);
        let (b, _) = report_for(42, 64);
        assert_eq!(a.to_json(), b.to_json());
    }

    #[test]
    fn json_changes_when_the_seed_changes() {
        let (a, _) = report_for(1, 64);
        let (b, _) = report_for(2, 64);
        assert_ne!(a.to_json(), b.to_json());
    }

    #[test]
    fn every_host_converges_and_none_are_stranded_on_a_clean_run() {
        let (r, _) = report_for(7, 128);
        assert!(r.is_success());
        assert_eq!(r.converged_hosts(), 128);
        assert_eq!(r.stranded_hosts(), 0);
        assert!(r.time_to_converge.is_some());
    }

    #[test]
    fn throughput_is_reported_for_every_edge_that_fired() {
        let (r, cfg) = report_for(3, 32);
        // 32 hosts, 1 seed -> 31 copy edges.
        assert_eq!(r.edge_throughput.len(), 31);
        for e in &r.edge_throughput {
            assert_eq!(e.bytes, cfg.closure_bytes);
            assert!(e.goodput_bytes_per_sec() > 0.0);
        }
        assert!(r.mean_goodput().unwrap() > 0.0);
        assert!(r.min_goodput().unwrap() > 0.0);
    }

    #[test]
    fn report_carries_all_five_schema_metric_families() {
        let (r, _) = report_for(5, 64);
        let json = r.to_json();
        for key in [
            "\"schema_version\"",
            "\"fan_out\"",
            "\"time_to_converge_ms\"",
            "\"throughput\"",
            "\"failure_subtree\"",
            "\"phase_timings\"",
        ] {
            assert!(json.contains(key), "missing {key} in report");
        }
    }

    #[test]
    fn markdown_summary_reports_the_headline_numbers() {
        let (r, _) = report_for(9, 64);
        let md = r.to_markdown();
        assert!(md.contains("seed `9`"));
        assert!(md.contains("64 hosts"));
        assert!(md.contains("depth 6"));
    }
}
