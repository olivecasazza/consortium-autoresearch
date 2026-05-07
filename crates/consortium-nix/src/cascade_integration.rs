//! Glue between the cascade primitive and the per-host fleet deploy.
//!
//! Production deploys build per-host toplevels (`build:hp01`, `build:hp02`,
//! ...). Each toplevel typically shares a large subgraph with the others
//! — same nixpkgs base, same kernel, etc. — but the top-of-graph paths
//! differ per host config.
//!
//! The cascade primitive distributes ONE store path to MANY hosts. To
//! drive it from a heterogeneous fleet, we group targets by their built
//! toplevel and run one cascade per group:
//!
//! - Homogeneous fleet (e.g. `mm01-mm05` all on the same Mac Mini config)
//!   → 1 cascade, log-N fan-out, big win.
//! - Heterogeneous fleet (every host different) → N cascades of size 1
//!   each, which is just direct copy. Same as today's behavior — no
//!   regression.
//! - Realistic mid-case (3 hp + 5 mm in one deploy, with 2 unique
//!   toplevels) → 2 cascades, both fan out within their group.
//!
//! Each cascade's seed is the host running cast — typically the dev box,
//! which already has the closure built locally. That maps cleanly to
//! `NixCopyExecutor` whose seed-edge runs `nix copy` LOCALLY (no SSH wrap).
//!
//! ## NodeId discipline
//!
//! [`LevelTreeFanOut`] (and the strategy contract in general) assumes
//! a *dense* NodeId space — `next_round` iterates `0..n_nodes`, and the
//! heap-tree parent math `(i-1)/fanout` only resolves to existing nodes
//! when ids are 0..n contiguous. So each per-toplevel cascade allocates
//! its own dense NodeIds locally (seed=0, targets=1..k).
//!
//! For the unified renderer we then *remap* events at the sink boundary:
//! every group's local NodeId(0) maps to one shared global seed id, and
//! every local target id maps to a globally-unique target id. The
//! renderer sees a single tree spanning all groups.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use crate::cascade_events::{CascadeEvent, Edge, EventSink, NullSink};
use crate::cascade_executor::NixCopyExecutor;
use crate::cascade_strategies::LevelTreeFanOut;

/// Translates per-group local NodeIds to globally-unique NodeIds at the
/// event-sink boundary. Each per-toplevel cascade uses its own dense
/// 0..k NodeId space (the strategy requires it); the renderer wants
/// every host to have a unique id across the whole deploy. This sink
/// sits between them.
///
/// Also suppresses per-group [`CascadeEvent::Started`] /
/// [`CascadeEvent::Finished`] — the parent [`cascade_copy_grouped`]
/// emits exactly one unified pair spanning all groups. Without
/// suppression, the renderer would reset its total-node count each
/// time a group started.
struct RemappingSink<'a> {
    inner: &'a dyn EventSink,
    /// Local NodeId → global NodeId. Local NodeId(0) (the seed) maps
    /// to the shared global seed id; each local target id maps to a
    /// pre-allocated globally-unique id.
    local_to_global: HashMap<NodeId, NodeId>,
}

impl<'a> RemappingSink<'a> {
    fn map(&self, id: NodeId) -> NodeId {
        // Fall back to identity — the cascade should only emit events
        // for ids it knows about, but a missing mapping shouldn't
        // hard-crash a deploy.
        self.local_to_global.get(&id).copied().unwrap_or(id)
    }
}

impl<'a> EventSink for RemappingSink<'a> {
    fn emit(&self, event: &CascadeEvent) {
        match event {
            // Suppressed — the parent emits unified Started + Finished
            // with global NodeIds and total counts spanning every group.
            CascadeEvent::Started { .. } | CascadeEvent::Finished { .. } => {}

            CascadeEvent::PlanComputed { round, assignments } => {
                let mapped: Vec<Edge> = assignments
                    .iter()
                    .map(|e| Edge {
                        src: self.map(e.src),
                        tgt: self.map(e.tgt),
                    })
                    .collect();
                self.inner.emit(&CascadeEvent::PlanComputed {
                    round: *round,
                    assignments: mapped,
                });
            }
            CascadeEvent::EdgeStarted {
                round,
                src,
                tgt,
                at,
            } => {
                self.inner.emit(&CascadeEvent::EdgeStarted {
                    round: *round,
                    src: self.map(*src),
                    tgt: self.map(*tgt),
                    at: *at,
                });
            }
            CascadeEvent::EdgeCompleted {
                round,
                src,
                tgt,
                duration,
            } => {
                self.inner.emit(&CascadeEvent::EdgeCompleted {
                    round: *round,
                    src: self.map(*src),
                    tgt: self.map(*tgt),
                    duration: *duration,
                });
            }
            CascadeEvent::EdgeFailed {
                round,
                src,
                tgt,
                error,
            } => {
                // The error itself contains NodeIds, but they're only
                // ever read for Display — the renderer routes the
                // event by src/tgt. Don't bother rewriting the inner
                // CascadeError tree.
                self.inner.emit(&CascadeEvent::EdgeFailed {
                    round: *round,
                    src: self.map(*src),
                    tgt: self.map(*tgt),
                    error: error.clone(),
                });
            }
            CascadeEvent::RoundCompleted {
                round,
                duration,
                has_closure,
            } => {
                let mapped: Vec<NodeId> = has_closure.iter().map(|id| self.map(*id)).collect();
                self.inner.emit(&CascadeEvent::RoundCompleted {
                    round: *round,
                    duration: *duration,
                    has_closure: mapped,
                });
            }
        }
    }
}

/// Per-host input to the grouped cascade copy.
#[derive(Debug, Clone)]
pub struct CascadeCopyTarget {
    /// Fleet name, e.g. "hp01".
    pub host_name: String,
    /// SSH addr, e.g. "root@hp01" or "root@192.168.1.121".
    pub ssh_addr: String,
    /// Toplevel store path produced by the build phase.
    pub toplevel_path: String,
}

/// Result of one grouped-cascade copy run.
#[derive(Debug, Default)]
pub struct CascadeCopyResult {
    /// Hosts whose toplevels successfully reached them.
    pub copied: Vec<String>,
    /// Per-host failure reason. Includes both transient-exhausted
    /// failures and orphan re-routing failures.
    pub failed: HashMap<String, String>,
}

/// Configuration for one grouped cascade copy.
pub struct CascadeCopyConfig<'a> {
    /// Where the closures originate. The host running cast IS the seed
    /// — `seed_addr` is just for display; the [`NixCopyExecutor`] uses
    /// local `nix copy` for any edge whose source is the seed.
    pub seed_addr: String,
    /// All targets from this deploy run. Will be grouped by toplevel.
    pub targets: Vec<CascadeCopyTarget>,
    /// Children-per-node in the F-ary tree. 2 = binary; bigger trades
    /// per-source bandwidth contention for fewer rounds. Default 2.
    pub fanout: u32,
    /// Per-edge `nix copy` timeout. Default 5min.
    pub timeout: Duration,
    /// Optional event sink for live UI. Use [`NullSink`] for headless.
    /// Reused across all groups via a [`RemappingSink`] wrapper that
    /// translates per-group local NodeIds into globally-unique ones.
    pub events: Option<&'a dyn EventSink>,
}

impl<'a> CascadeCopyConfig<'a> {
    pub fn new(seed_addr: impl Into<String>, targets: Vec<CascadeCopyTarget>) -> Self {
        Self {
            seed_addr: seed_addr.into(),
            targets,
            fanout: 2,
            timeout: Duration::from_secs(300),
            events: None,
        }
    }

    pub fn fanout(mut self, n: u32) -> Self {
        self.fanout = n.max(1);
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn events(mut self, sink: &'a dyn EventSink) -> Self {
        self.events = Some(sink);
        self
    }
}

/// Group `targets` by their `toplevel_path`, then run one cascade per
/// group **in parallel**. Returns the union of results across all groups.
///
/// # Behavior
///
/// - Empty `targets` → empty result, no work.
/// - All targets share one toplevel → 1 cascade, N targets, full
///   peer-to-peer fan-out — the maximum cascade win.
/// - All targets unique toplevels → N cascades of 1 target each, all
///   running concurrently — equivalent to today's parallel `nix copy`.
/// - Mixed (e.g. 5 mm + 3 hp where mm share one toplevel and each hp
///   is unique) → 4 cascades running concurrently, the mm group fans
///   out internally.
///
/// # Concurrency
///
/// Each group runs on its own OS thread via [`std::thread::scope`].
/// Each cascade's [`NixCopyExecutor`] internally spawns one thread per
/// edge per round. Total in-flight subprocess count peaks at
/// `Σ_groups (group_targets / fanout)` which is bounded by the total
/// fleet size — same as the old parallel-copy path.
///
/// # Strategy
///
/// Currently hardcoded to [`LevelTreeFanOut`]. The `MaxBottleneckSpanning`
/// / `SteinerGreedy` strategies require a populated `NetworkProfile`
/// which we don't gather for production deploys (would need an active
/// bandwidth probe step). When that lands, swap the strategy here.
pub fn cascade_copy_grouped(cfg: CascadeCopyConfig<'_>) -> CascadeCopyResult {
    if cfg.targets.is_empty() {
        return CascadeCopyResult::default();
    }

    // Group targets by their toplevel path.
    let mut groups_map: HashMap<String, Vec<CascadeCopyTarget>> = HashMap::new();
    for t in cfg.targets {
        groups_map
            .entry(t.toplevel_path.clone())
            .or_default()
            .push(t);
    }

    // Allocate GLOBAL NodeIds — one shared seed id, one per target.
    // These are the IDs the renderer sees. Strategy/cascade execution
    // uses fresh dense local IDs per group; the RemappingSink bridges.
    let mut alloc = NodeIdAlloc::new();
    let global_seed = alloc.alloc();

    // For each group: pair each target with its global NodeId.
    let mut groups: Vec<(String, Vec<(CascadeCopyTarget, NodeId)>)> = Vec::new();
    for (toplevel, targets) in groups_map {
        let with_global: Vec<(CascadeCopyTarget, NodeId)> =
            targets.into_iter().map(|t| (t, alloc.alloc())).collect();
        groups.push((toplevel, with_global));
    }

    // Total node count for the unified Started event.
    let n_total: u32 = groups.iter().map(|(_, g)| g.len() as u32).sum::<u32>() + 1;

    let strategy = LevelTreeFanOut::new(cfg.fanout);
    let null_sink = NullSink;
    let user_events: &dyn EventSink = cfg.events.unwrap_or(&null_sink);

    // Emit ONE Started event for the whole grouped cascade. Per-group
    // cascades' Started events get suppressed by the per-group
    // RemappingSink so they don't clobber the unified total_nodes count.
    user_events.emit(&CascadeEvent::Started {
        n_nodes: n_total,
        seeded: vec![global_seed],
        strategy: format!("LevelTreeFanOut(fanout={})", cfg.fanout),
        at: SystemTime::now(),
    });

    use std::sync::Mutex;
    let result_mtx = Mutex::new(CascadeCopyResult::default());

    std::thread::scope(|scope| {
        for (toplevel, group_with_global) in groups {
            let seed_addr = cfg.seed_addr.clone();
            let strategy_ref = &strategy;
            let result_ref = &result_mtx;
            let user_events_ref: &dyn EventSink = user_events;
            scope.spawn(move || {
                let mut local = CascadeCopyResult::default();
                run_one_group(
                    &seed_addr,
                    global_seed,
                    &toplevel,
                    group_with_global,
                    strategy_ref,
                    user_events_ref,
                    cfg.timeout,
                    &mut local,
                );
                let mut shared = result_ref.lock().unwrap();
                shared.copied.extend(local.copied);
                for (h, e) in local.failed {
                    shared.failed.insert(h, e);
                }
            });
        }
    });

    let result = result_mtx.into_inner().unwrap();

    // Emit unified Finished. converged = seed + every host that copied;
    // failed = unique hosts that didn't.
    user_events.emit(&CascadeEvent::Finished {
        converged: result.copied.len() + 1,
        failed: result.failed.len(),
        rounds: 0, // multi-cascade: meaningful per-group, not unified
    });

    result
}

/// Run one per-toplevel cascade. Inside the cascade, NodeIds are dense
/// (seed=0, targets=1..k) so [`LevelTreeFanOut`]'s heap-tree math and
/// `0..n_nodes` iteration both work. At the event-sink boundary we
/// remap these local IDs to the globally-unique IDs in `global_targets`,
/// so the unified renderer sees one tree across all parallel groups.
#[allow(clippy::too_many_arguments)]
fn run_one_group(
    seed_addr: &str,
    global_seed: NodeId,
    toplevel: &str,
    group_with_global: Vec<(CascadeCopyTarget, NodeId)>,
    strategy: &LevelTreeFanOut,
    user_events: &dyn EventSink,
    timeout: Duration,
    result: &mut CascadeCopyResult,
) {
    // Allocate dense local NodeIds for this cascade's strategy view.
    let mut alloc = NodeIdAlloc::new();
    let local_seed = alloc.alloc();

    let mut cascade_nodes: Vec<CascadeNode> =
        vec![CascadeNode::new(local_seed, seed_addr.to_string())];
    let mut addrs: HashMap<NodeId, String> = HashMap::new();
    addrs.insert(local_seed, seed_addr.to_string());

    // Local-id-keyed maps for routing results back to host names, plus
    // the local→global NodeId mapping the RemappingSink needs.
    let mut local_to_host: HashMap<NodeId, String> = HashMap::new();
    let mut local_to_global: HashMap<NodeId, NodeId> = HashMap::new();
    local_to_global.insert(local_seed, global_seed);

    for (target, global_id) in &group_with_global {
        let local_id = alloc.alloc();
        cascade_nodes.push(CascadeNode::new(local_id, target.ssh_addr.clone()));
        addrs.insert(local_id, target.ssh_addr.clone());
        local_to_host.insert(local_id, target.host_name.clone());
        local_to_global.insert(local_id, *global_id);
    }

    let mut seeded = HashSet::new();
    seeded.insert(local_seed);

    let executor =
        NixCopyExecutor::new(addrs, toplevel.to_string(), local_seed).with_timeout(timeout);

    // Wrap the user's event sink so events from THIS group get their
    // local NodeIds rewritten to global ones before the renderer sees
    // them. Also suppresses per-group Started/Finished — the parent
    // emits exactly one unified pair spanning all groups.
    let sink = RemappingSink {
        inner: user_events,
        local_to_global,
    };

    let cascade_result = Cascade::new()
        .nodes(cascade_nodes)
        .seeded(seeded)
        .network(NetworkProfile::default())
        .strategy(strategy)
        .executor(&executor)
        .events(&sink)
        .run();

    for id in &cascade_result.converged {
        if *id == local_seed {
            continue;
        }
        if let Some(host) = local_to_host.get(id) {
            result.copied.push(host.clone());
        }
    }
    if let Some(err) = cascade_result.failed {
        let msg = format!("{}", err);
        let mut seen = HashSet::new();
        for affected_id in err.affected_nodes() {
            if !seen.insert(affected_id) {
                continue;
            }
            if let Some(host) = local_to_host.get(&affected_id) {
                result.failed.insert(host.clone(), msg.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, addr: &str, tl: &str) -> CascadeCopyTarget {
        CascadeCopyTarget {
            host_name: name.into(),
            ssh_addr: addr.into(),
            toplevel_path: tl.into(),
        }
    }

    #[test]
    fn empty_targets_returns_empty() {
        let cfg = CascadeCopyConfig::new("root@seed", vec![]);
        let r = cascade_copy_grouped(cfg);
        assert!(r.copied.is_empty());
        assert!(r.failed.is_empty());
    }

    /// Heterogeneous deploy: 5 unique-toplevel targets all run as
    /// 5 concurrent 1-node cascades. Verifies that:
    ///   - every group's local NodeId(1) gets remapped to its OWN
    ///     unique global id (no collisions),
    ///   - the strategy's dense-id assumption isn't violated by the
    ///     unified-renderer split (the bug in commit 259f75c).
    #[test]
    fn heterogeneous_targets_each_get_unique_global_ids() {
        use crate::cascade_events::{CascadeEvent, EventSink};
        use std::sync::Mutex;

        struct Capture(Mutex<Vec<CascadeEvent>>);
        impl EventSink for Capture {
            fn emit(&self, event: &CascadeEvent) {
                self.0.lock().unwrap().push(event.clone());
            }
        }
        let cap = Capture(Mutex::new(Vec::new()));

        let targets = vec![
            t("a", "root@a", "/nix/store/aaa"),
            t("b", "root@b", "/nix/store/bbb"),
            t("c", "root@c", "/nix/store/ccc"),
        ];
        let cfg = CascadeCopyConfig::new("root@seed", targets).events(&cap);

        // We can't actually run nix copy in tests — the cascade will
        // fail every edge with `failed_to_start_subprocess` style
        // errors. That's fine: we're checking event NodeIds, not
        // success. Use a 100ms timeout so the test doesn't hang.
        let _ = cascade_copy_grouped(cfg.timeout(Duration::from_millis(100)));

        let events = cap.0.lock().unwrap();

        // Find every PlanComputed assignment. Across all 3 groups,
        // every tgt id should be unique (no two groups reusing
        // NodeId(1)).
        let mut all_tgts: Vec<NodeId> = Vec::new();
        for ev in events.iter() {
            if let CascadeEvent::PlanComputed { assignments, .. } = ev {
                for e in assignments {
                    all_tgts.push(e.tgt);
                }
            }
        }
        // 3 separate groups should produce 3 distinct target NodeIds in
        // events (the cascade may retry-emit each one many times — what
        // matters is the *unique* set across all emissions, since a
        // single shared id (the bug) would only ever produce 1 unique
        // value regardless of how many groups ran).
        let unique: std::collections::HashSet<_> = all_tgts.iter().copied().collect();
        assert_eq!(
            unique.len(),
            3,
            "expected 3 distinct global target NodeIds across the 3 groups; got {} (raw stream: {:?})",
            unique.len(),
            all_tgts,
        );
        // And the seed (NodeId 0) MUST NOT appear as a target.
        assert!(
            !unique.contains(&NodeId(0)),
            "seed NodeId(0) should not appear as an edge target"
        );

        // Exactly ONE Started event with n_nodes = 4 (1 seed + 3 targets).
        let started: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, CascadeEvent::Started { .. }))
            .collect();
        assert_eq!(
            started.len(),
            1,
            "expected 1 unified Started, got {}",
            started.len()
        );
        if let CascadeEvent::Started { n_nodes, .. } = started[0] {
            assert_eq!(*n_nodes, 4);
        }

        // Exactly ONE Finished event.
        let finished: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, CascadeEvent::Finished { .. }))
            .collect();
        assert_eq!(finished.len(), 1, "expected 1 unified Finished");
    }
}
