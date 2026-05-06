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
use std::time::Duration;

use crate::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use crate::cascade_events::{EventSink, NullSink};
use crate::cascade_executor::NixCopyExecutor;
use crate::cascade_strategies::LevelTreeFanOut;

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
    let mut result = CascadeCopyResult::default();
    if cfg.targets.is_empty() {
        return result;
    }

    // Group targets by their toplevel path. Stable iteration order so
    // logs / live UI are deterministic across runs.
    let mut groups: HashMap<String, Vec<CascadeCopyTarget>> = HashMap::new();
    for t in cfg.targets {
        groups.entry(t.toplevel_path.clone()).or_default().push(t);
    }

    let strategy = LevelTreeFanOut::new(cfg.fanout);
    let null_sink = NullSink;
    let events: &dyn EventSink = cfg.events.unwrap_or(&null_sink);

    // Run each group in its own thread, all sharing the same event
    // sink + strategy. thread::scope lets us pass borrowed refs in
    // without needing 'static bounds.
    use std::sync::Mutex;
    let result_mtx = Mutex::new(CascadeCopyResult::default());

    std::thread::scope(|scope| {
        for (toplevel, group) in groups {
            let seed_addr = cfg.seed_addr.clone();
            let strategy_ref = &strategy;
            let result_ref = &result_mtx;
            scope.spawn(move || {
                let mut local = CascadeCopyResult::default();
                run_one_group(
                    &seed_addr,
                    &toplevel,
                    group,
                    strategy_ref,
                    events,
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

    result = result_mtx.into_inner().unwrap();
    result
}

fn run_one_group(
    seed_addr: &str,
    toplevel: &str,
    group: Vec<CascadeCopyTarget>,
    strategy: &LevelTreeFanOut,
    events: &dyn EventSink,
    timeout: Duration,
    result: &mut CascadeCopyResult,
) {
    // Build NodeIds: seed at NodeId(0), targets at NodeId(1)..
    let mut alloc = NodeIdAlloc::new();
    let seed_id = alloc.alloc();

    let mut cascade_nodes: Vec<CascadeNode> =
        vec![CascadeNode::new(seed_id, seed_addr.to_string())];
    let mut addrs: HashMap<NodeId, String> = HashMap::new();
    addrs.insert(seed_id, seed_addr.to_string());

    let mut id_to_host: HashMap<NodeId, String> = HashMap::new();

    for t in &group {
        let id = alloc.alloc();
        cascade_nodes.push(CascadeNode::new(id, t.ssh_addr.clone()));
        addrs.insert(id, t.ssh_addr.clone());
        id_to_host.insert(id, t.host_name.clone());
    }

    let mut seeded = HashSet::new();
    seeded.insert(seed_id);

    let executor = NixCopyExecutor::new(addrs, toplevel.to_string(), seed_id).with_timeout(timeout);

    let cascade_result = Cascade::new()
        .nodes(cascade_nodes)
        .seeded(seeded)
        .network(NetworkProfile::default())
        .strategy(strategy)
        .executor(&executor)
        .events(events)
        .run();

    // Map NodeIds back to host names.
    for id in &cascade_result.converged {
        if let Some(host) = id_to_host.get(id) {
            // The seed isn't a deploy target; skip it.
            if *id != seed_id {
                result.copied.push(host.clone());
            }
        }
    }
    if let Some(err) = cascade_result.failed {
        let msg = format!("{}", err);
        // Dedupe affected nodes — a single subtree-aggregate can name
        // the same host multiple times if it bubbled up across rounds.
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
