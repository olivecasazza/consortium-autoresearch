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

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use crate::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use crate::cascade_events::{CascadeEvent, EventSink, NullSink};
use crate::cascade_executor::NixCopyExecutor;
use crate::cascade_strategies::LevelTreeFanOut;

/// Forwards every cascade event EXCEPT [`CascadeEvent::Started`] and
/// [`CascadeEvent::Finished`]. Used by [`cascade_copy_grouped`] so each
/// per-toplevel sub-cascade doesn't reset the renderer's total-node
/// count or fire a premature "done" frame — the parent emits exactly
/// one Started + Finished spanning all groups.
struct DropStartFinish<'a>(&'a dyn EventSink);

impl<'a> EventSink for DropStartFinish<'a> {
    fn emit(&self, event: &CascadeEvent) {
        match event {
            CascadeEvent::Started { .. } | CascadeEvent::Finished { .. } => {
                // Suppress — outer cascade_copy_grouped emits unified versions.
            }
            _ => self.0.emit(event),
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
    /// Reused across all groups (the renderer can handle multiple
    /// cascade runs back-to-back, though it'll show them sequentially).
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

    // Allocate GLOBAL NodeIds across all groups so the renderer sees
    // a single unified tree. NodeId(0) is the shared virtual seed
    // (same across all groups — same physical box). Each target gets
    // its own contiguous global NodeId, regardless of which toplevel
    // group it belongs to. Without this, every per-group cascade
    // would reuse NodeId(0)/NodeId(1)/... and the renderer collapses
    // them into a single edge.
    let mut alloc = NodeIdAlloc::new();
    let global_seed = alloc.alloc();

    // For each group: pair each target with its global NodeId.
    let mut groups: Vec<(String, Vec<(CascadeCopyTarget, NodeId)>)> = Vec::new();
    for (toplevel, targets) in groups_map {
        let with_ids: Vec<(CascadeCopyTarget, NodeId)> =
            targets.into_iter().map(|t| (t, alloc.alloc())).collect();
        groups.push((toplevel, with_ids));
    }

    // Total node count for the unified Started event.
    let n_total: u32 = groups.iter().map(|(_, g)| g.len() as u32).sum::<u32>() + 1;

    let strategy = LevelTreeFanOut::new(cfg.fanout);
    let null_sink = NullSink;
    let user_events: &dyn EventSink = cfg.events.unwrap_or(&null_sink);

    // Emit ONE Started event for the whole grouped cascade. Per-group
    // cascades' Started events get suppressed by DropStartFinish so
    // they don't clobber the unified total_nodes count.
    user_events.emit(&CascadeEvent::Started {
        n_nodes: n_total,
        seeded: vec![global_seed],
        strategy: format!("LevelTreeFanOut(fanout={})", cfg.fanout),
        at: SystemTime::now(),
    });

    let group_events = DropStartFinish(user_events);

    use std::sync::Mutex;
    let result_mtx = Mutex::new(CascadeCopyResult::default());

    std::thread::scope(|scope| {
        for (toplevel, group_with_ids) in groups {
            let seed_addr = cfg.seed_addr.clone();
            let strategy_ref = &strategy;
            let result_ref = &result_mtx;
            let events_ref: &dyn EventSink = &group_events;
            scope.spawn(move || {
                let mut local = CascadeCopyResult::default();
                run_one_group_global(
                    &seed_addr,
                    global_seed,
                    &toplevel,
                    group_with_ids,
                    strategy_ref,
                    events_ref,
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

/// Run one per-toplevel cascade using GLOBAL NodeIds assigned by
/// the parent [`cascade_copy_grouped`]. Each group cascade still has
/// its own independent [`Cascade`] graph (one seed + its own targets),
/// but the NodeIds are unique across all concurrent groups so the
/// shared event sink/renderer sees a single unified tree.
///
/// The seed NodeId is the same `global_seed` across all groups —
/// they all originate from the same physical box, and the renderer
/// shows them as one shared root with each group's targets as
/// children.
fn run_one_group_global(
    seed_addr: &str,
    global_seed: NodeId,
    toplevel: &str,
    group: Vec<(CascadeCopyTarget, NodeId)>,
    strategy: &LevelTreeFanOut,
    events: &dyn EventSink,
    timeout: Duration,
    result: &mut CascadeCopyResult,
) {
    let mut cascade_nodes: Vec<CascadeNode> =
        vec![CascadeNode::new(global_seed, seed_addr.to_string())];
    let mut addrs: HashMap<NodeId, String> = HashMap::new();
    addrs.insert(global_seed, seed_addr.to_string());

    let mut id_to_host: HashMap<NodeId, String> = HashMap::new();

    for (t, id) in &group {
        cascade_nodes.push(CascadeNode::new(*id, t.ssh_addr.clone()));
        addrs.insert(*id, t.ssh_addr.clone());
        id_to_host.insert(*id, t.host_name.clone());
    }

    let mut seeded = HashSet::new();
    seeded.insert(global_seed);

    let executor =
        NixCopyExecutor::new(addrs, toplevel.to_string(), global_seed).with_timeout(timeout);

    let cascade_result = Cascade::new()
        .nodes(cascade_nodes)
        .seeded(seeded)
        .network(NetworkProfile::default())
        .strategy(strategy)
        .executor(&executor)
        .events(events)
        .run();

    for id in &cascade_result.converged {
        if *id == global_seed {
            continue;
        }
        if let Some(host) = id_to_host.get(id) {
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
            if let Some(host) = id_to_host.get(&affected_id) {
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

    // End-to-end behavior is exercised by the cascade_executor tests
    // (which spawn real subprocesses). Group bookkeeping is exercised
    // by the empty-targets case + manual smoke via cascade-copy bin.
    // A pure-bookkeeping test would need a faked NixCopyExecutor —
    // leaving for when somebody hits the "what if nix copy returned
    // <weird thing>" question and needs the seam.
}
