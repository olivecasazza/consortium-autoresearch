//! Deterministic simulator for the nix-deploy DAG.
//!
//! [`consortium_nix::deploy`] builds a five-stage pipeline per host:
//! `eval` (local `nix flake` evaluation), `health` (builder probe),
//! `build` (produce the closure), `copy` (push it to the target), and
//! `activate` (switch profiles). The real implementation hard-codes
//! that stage list and its concurrency limits inline, so nothing can
//! ask "what would this cost at 1024 nodes, with 3% packet loss and a
//! 100 MB/s uplink?" without actually running `nix` and `ssh`.
//!
//! This module answers that question two ways, from one cost model:
//!
//! - [`DeployDagSim::run`] is a **deterministic virtual scheduler**. It
//!   computes per-stage cost analytically, then list-schedules it
//!   against the same concurrency limits the real deploy uses. No
//!   threads, no clock, no I/O — the same seed always yields a
//!   byte-identical [`DagSimReport`], including per-stage makespans and
//!   convergence counts.
//! - [`DeployDagSim::validate`] builds the **real**
//!   [`consortium::dag::StageBuilder`] graph with synthetic tasks and
//!   runs the real [`consortium::dag::DagExecutor`]. It answers a
//!   different question: does the real dependency graph, concurrency
//!   group enforcement, and error policy actually behave the way the
//!   cost model assumes? It returns the real [`DagReport`].
//!
//! Splitting the two is deliberate. Wall-clock timing under a real
//! thread pool is not reproducible, so it cannot be the thing we assert
//! on. Structural behaviour *is* reproducible, so the real executor
//! checks that, and the virtual scheduler supplies the numbers.
//!
//! # Time model
//!
//! There is no `now`. Each stage's cost is a pure function of
//! [`DagSimConfig`], so scheduling is arithmetic on a virtual timeline:
//! a host's stage *k* cannot start before its stage *k-1* finished, and
//! at most `limit(k)` tasks of stage *k* may be in flight. The
//! scheduler walks stages in pipeline order and, within a stage, hosts
//! in index order, always taking the slot that frees up earliest. That
//! fixed order plus earliest-free-slot selection is what makes the
//! result deterministic; see [`DeployDagSim::run`].

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use consortium::dag::{
    DagContext, DagReport, DagTask, ErrorPolicy, StageBuilder, TaskId, TaskOutcome,
};
use consortium_nix::cascade::{NetworkProfile, NodeId, NodeSpec};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use crate::fixtures::{rng_from_seed, BandwidthDistribution, UplinkDistribution};
use crate::link::LinkModel;

/// One stage of the deploy pipeline, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeployStage {
    /// Local `nix flake` evaluation. Serial in production because it
    /// is one control-node process.
    Eval,
    /// Builder health probe, mirroring
    /// [`consortium_nix::health::check_builders`]. Gates the build.
    Health,
    /// Produce the closure on a builder.
    Build,
    /// Push the closure to the target over the fabric.
    Copy,
    /// Switch the target's profile.
    Activate,
}

impl DeployStage {
    /// Pipeline order. Index into a fixed-size array.
    pub const ALL: [DeployStage; 5] = [
        DeployStage::Eval,
        DeployStage::Health,
        DeployStage::Build,
        DeployStage::Copy,
        DeployStage::Activate,
    ];

    /// Stage name as it appears in the real `TaskId` (`"eval:host-0"`).
    pub fn name(self) -> &'static str {
        match self {
            DeployStage::Eval => "eval",
            DeployStage::Health => "health",
            DeployStage::Build => "build",
            DeployStage::Copy => "copy",
            DeployStage::Activate => "activate",
        }
    }

    /// Index into [`DeployStage::ALL`].
    pub fn index(self) -> usize {
        match self {
            DeployStage::Eval => 0,
            DeployStage::Health => 1,
            DeployStage::Build => 2,
            DeployStage::Copy => 3,
            DeployStage::Activate => 4,
        }
    }

    /// Parse a stage name back. Returns `None` for unknown names.
    pub fn from_name(name: &str) -> Option<DeployStage> {
        DeployStage::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// Per-stage concurrency limits.
///
/// The defaults mirror `consortium_nix`'s `deploy()`: evaluation is
/// serial (`eval_limit = 1`) because it is a single control-node
/// process, and activation is capped at 4 because profile switches
/// contend on shared `/nix/var` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSchedule {
    pub eval: usize,
    pub health: usize,
    pub build: usize,
    pub copy: usize,
    pub activate: usize,
}

impl StageSchedule {
    /// The production limits for a deploy with `max_parallel` workers.
    pub fn production(max_parallel: usize) -> Self {
        let p = max_parallel.max(1);
        Self {
            eval: 1,
            health: p,
            build: p,
            copy: p,
            activate: p.min(4),
        }
    }

    /// Concurrency limit for one stage.
    pub fn limit(self, stage: DeployStage) -> usize {
        match stage {
            DeployStage::Eval => self.eval,
            DeployStage::Health => self.health,
            DeployStage::Build => self.build,
            DeployStage::Copy => self.copy,
            DeployStage::Activate => self.activate,
        }
    }
}

impl Default for StageSchedule {
    fn default() -> Self {
        Self::production(8)
    }
}

/// Fixed, host-independent cost knobs per stage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageCost {
    /// Local evaluation of one host's system.
    pub eval: Duration,
    /// One builder health probe (SSH + `nix store ping`).
    pub health_probe: Duration,
    /// Bytes a builder must produce per host. Drives `build` cost.
    pub build_bytes: u64,
    /// Profile switch on the target.
    pub activate: Duration,
}

impl Default for StageCost {
    fn default() -> Self {
        Self {
            eval: Duration::from_millis(250),
            health_probe: Duration::from_millis(40),
            build_bytes: 8 * 1024 * 1024,
            activate: Duration::from_millis(900),
        }
    }
}

/// Deterministic failures injected into the deploy DAG.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DagFailures {
    /// Nothing fails.
    #[default]
    None,
    /// Every `every`-th host (0-based index) fails at `stage`.
    EveryNthHost {
        stage: DeployStage,
        every: usize,
        reason: String,
    },
    /// The named hosts (by index) fail at `stage`.
    Hosts {
        stage: DeployStage,
        hosts: BTreeSet<usize>,
        reason: String,
    },
}

impl DagFailures {
    /// Whether `host` fails at `stage`, and why.
    pub fn check(&self, stage: DeployStage, host: usize) -> Option<&str> {
        match self {
            DagFailures::None => None,
            DagFailures::EveryNthHost {
                stage: s,
                every,
                reason,
            } => {
                let every = (*every).max(1);
                if *s == stage && host % every == 0 {
                    Some(reason)
                } else {
                    None
                }
            }
            DagFailures::Hosts {
                stage: s,
                hosts,
                reason,
            } => {
                if *s == stage && hosts.contains(&host) {
                    Some(reason)
                } else {
                    None
                }
            }
        }
    }
}

/// Network shape for the deploy DAG.
///
/// Populated onto a [`NetworkProfile`] the same way the cascade's
/// [`crate::fixtures`] do, so a builder-side link is described by the
/// same knobs the cascade already uses.
#[derive(Debug, Clone)]
pub struct DagNetwork {
    /// Bandwidth of every directed edge between the modelled nodes.
    pub bandwidth: BandwidthDistribution,
    /// Per-node uplink/downlink capacities. `None` disables
    /// contention modelling, matching the cascade default.
    pub uplinks: Option<UplinkDistribution>,
    /// Nominal one-way link latency.
    pub latency: Duration,
}

impl Default for DagNetwork {
    fn default() -> Self {
        Self {
            bandwidth: BandwidthDistribution::Uniform(100 * 1024 * 1024),
            uplinks: None,
            latency: Duration::from_millis(2),
        }
    }
}

/// Everything needed to simulate one deploy DAG run.
#[derive(Debug, Clone)]
pub struct DagSimConfig {
    /// Seed for every stochastic draw. Same seed ⇒ identical report.
    pub seed: u64,
    /// Number of deployment targets.
    pub hosts: usize,
    /// Closure bytes pushed to each target during `copy`.
    pub closure_bytes: u64,
    /// Builder output bandwidth, bytes/sec, for `build`.
    pub builder_bandwidth: u64,
    /// Concurrency limits.
    pub schedule: StageSchedule,
    /// Fixed per-stage costs.
    pub cost: StageCost,
    /// Fabric shape.
    pub network: DagNetwork,
    /// Packet loss / jitter / direction.
    pub link: LinkModel,
    /// Relative per-host cost spread in `[0, 1)`. Models a fleet that
    /// is not homogeneous. `0.0` makes every host identical.
    pub host_variation: f64,
    /// Injected failures.
    pub failures: DagFailures,
}

impl Default for DagSimConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            hosts: 16,
            closure_bytes: 100 * 1024 * 1024,
            builder_bandwidth: 1024 * 1024 * 1024,
            schedule: StageSchedule::default(),
            cost: StageCost::default(),
            network: DagNetwork::default(),
            link: LinkModel::new(),
            host_variation: 0.25,
            failures: DagFailures::None,
        }
    }
}

impl DagSimConfig {
    /// Host name for a 0-based index, matching the cascade's naming.
    pub fn host_name(&self, idx: usize) -> String {
        format!("user@host-{}", idx)
    }

    /// NodeId of the builder. Placed after every target so it cannot
    /// collide with a target index.
    pub fn builder_id(&self) -> NodeId {
        NodeId(self.hosts as u32)
    }

    /// NodeId of a target host.
    pub fn host_id(&self, idx: usize) -> NodeId {
        NodeId(idx as u32)
    }

    /// Number of nodes the network profile covers: every target plus
    /// the builder.
    pub fn network_nodes(&self) -> u32 {
        self.hosts as u32 + 1
    }

    /// Build the [`NetworkProfile`] this config describes.
    pub fn network_profile(&self) -> NetworkProfile {
        let mut rng = rng_from_seed(self.seed ^ 0xD4E6_0000_0000_0001);
        let mut net = NetworkProfile::default();
        let n = self.network_nodes();
        self.network
            .bandwidth
            .populate(&mut rng, &mut net, n);
        if let Some(u) = &self.network.uplinks {
            u.populate(&mut rng, &mut net, n);
        }
        crate::fixtures::populate_uniform_latency(
            &mut net,
            self.network.latency,
            n,
        );
        net
    }
}

/// Timing and convergence for one stage.
#[derive(Debug, Clone, PartialEq)]
pub struct StageTiming {
    pub stage: DeployStage,
    /// Concurrency limit applied.
    pub limit: usize,
    /// `last_end - first_start` across the stage's tasks.
    pub makespan: Duration,
    /// Sum of every task's cost. Compare to `makespan` to see how much
    /// parallelism the limit actually bought.
    pub total_work: Duration,
    /// Maximum tasks that were simultaneously in flight, measured by
    /// an interval sweep — not merely `min(limit, hosts)`.
    pub peak_concurrency: usize,
    /// Hosts whose chain reached this stage. Drops after the stage
    /// where a failure was injected.
    pub converged_hosts: usize,
    /// `total_work / (makespan * peak_concurrency)`, in `(0, 1]`.
    /// `1.0` means the stage kept every slot busy the whole time.
    pub parallel_efficiency: f64,
}

impl StageTiming {
    /// Speedup over running the stage's tasks one at a time.
    pub fn speedup(&self) -> f64 {
        if self.makespan.is_zero() {
            return 1.0;
        }
        self.total_work.as_secs_f64() / self.makespan.as_secs_f64()
    }
}

/// Full result of a simulated deploy.
#[derive(Debug, Clone, PartialEq)]
pub struct DagSimReport {
    /// Number of hosts simulated.
    pub hosts: usize,
    /// Per-stage timing, in pipeline order.
    pub stages: Vec<StageTiming>,
    /// Makespan of the whole DAG.
    pub wall_time: Duration,
    /// Hosts that completed all five stages.
    pub converged_hosts: usize,
    /// `(stage, host)` pairs that failed.
    pub failures: Vec<(DeployStage, usize, String)>,
}

impl DagSimReport {
    /// Timing for one stage.
    pub fn stage(&self, stage: DeployStage) -> &StageTiming {
        self.stages
            .iter()
            .find(|s| s.stage == stage)
            .expect("every stage is reported")
    }

    /// Hosts that reached `stage` without being failed or
    /// downstream of a failure.
    pub fn converged_at(&self, stage: DeployStage) -> usize {
        self.stage(stage).converged_hosts
    }

    /// True when every host completed the full pipeline.
    pub fn is_success(&self) -> bool {
        self.failures.is_empty() && self.converged_hosts == self.hosts
    }

    /// A compact one-line-per-stage summary, for examples and reports.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "deploy sim: {} hosts, wall {:.3}s, converged {}/{}\n",
            self.hosts,
            self.wall_time.as_secs_f64(),
            self.converged_hosts,
            self.hosts
        );
        for t in &self.stages {
            s.push_str(&format!(
                "  {:<9} makespan {:>9.3}s  work {:>9.3}s  peak {:>3}/{}  eff {:>5.1}%  converged {}\n",
                t.stage.name(),
                t.makespan.as_secs_f64(),
                t.total_work.as_secs_f64(),
                t.peak_concurrency,
                t.limit,
                t.parallel_efficiency * 100.0,
                t.converged_hosts,
            ));
        }
        s
    }
}

/// Deterministic deploy-DAG simulator. See the [module docs](self).
pub struct DeployDagSim {
    cfg: DagSimConfig,
    net: NetworkProfile,
}

impl DeployDagSim {
    pub fn new(cfg: DagSimConfig) -> Self {
        let net = cfg.network_profile();
        Self { cfg, net }
    }

    pub fn config(&self) -> &DagSimConfig {
        &self.cfg
    }

    /// The network profile the cost model reads from.
    pub fn network(&self) -> &NetworkProfile {
        &self.net
    }

    /// Deterministic per-host cost multiplier in
    /// `[1 - spread, 1 + spread]`, drawn from the config seed.
    fn host_factor(&self, host: usize) -> f64 {
        let spread = self.cfg.host_variation.clamp(0.0, 0.95);
        if spread == 0.0 {
            return 1.0;
        }
        let mut rng = ChaCha8Rng::seed_from_u64(
            self.cfg.seed
                ^ 0x484F_5354_5641_5200u64
                ^ (host as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        );
        1.0 + rng.gen_range(-spread..=spread)
    }

    /// Cost of running `stage` on `host`, ignoring scheduling.
    ///
    /// This is the single source of truth for both [`Self::run`] and
    /// [`Self::validate`]; neither can drift from the other.
    pub fn stage_cost(&self, stage: DeployStage, host: usize) -> Duration {
        let f = self.host_factor(host);
        match stage {
            DeployStage::Eval => self.cfg.cost.eval.mul_f64(f),
            DeployStage::Health => {
                // A probe is a round trip on the real link, so it pays
                // jitter, then the probe itself.
                let builder = self.cfg.builder_id();
                let tgt = self.cfg.host_id(host);
                let lat = self
                    .cfg
                    .link
                    .jitter()
                    .sample(self.cfg.seed, builder, tgt, self.net.latency_of(builder, tgt, self.cfg.network.latency));
                lat + self.cfg.cost.health_probe.mul_f64(f)
            }
            DeployStage::Build => {
                let bytes = (self.cfg.cost.build_bytes as f64 * f) as u64;
                Duration::from_secs_f64(bytes as f64 / self.cfg.builder_bandwidth.max(1) as f64)
            }
            DeployStage::Copy => {
                let builder = self.cfg.builder_id();
                let tgt = self.cfg.host_id(host);
                // The builder pushes `copy_limit` streams at once; each
                // target takes exactly one. That is the contention the
                // limit implies, fed straight into the link model so
                // packet loss and asymmetry see the real fan-out.
                let src_out = self.cfg.schedule.copy.max(1) as u64;
                let tgt_in = 1u64;
                let edge_bw = self
                    .net
                    .bandwidth_of(builder, tgt, u64::MAX);
                let src_spec = self
                    .net
                    .nodes
                    .get(&builder)
                    .copied()
                    .unwrap_or(NodeSpec::symmetric(u64::MAX));
                let tgt_spec = self
                    .net
                    .nodes
                    .get(&tgt)
                    .copied()
                    .unwrap_or(NodeSpec::symmetric(u64::MAX));
                let bw = self.cfg.link.effective_bandwidth(
                    edge_bw,
                    src_spec.uplink,
                    tgt_spec.downlink,
                    src_out,
                    tgt_in,
                );
                self.cfg.link.transfer_time(
                    self.cfg.seed,
                    builder,
                    tgt,
                    self.cfg.closure_bytes,
                    bw,
                    self.net.latency_of(builder, tgt, self.cfg.network.latency),
                )
            }
            DeployStage::Activate => self.cfg.cost.activate.mul_f64(f),
        }
    }

    /// Every stage's cost for every host, as `costs[host][stage_index]`.
    fn cost_matrix(&self) -> Vec<[Duration; 5]> {
        (0..self.cfg.hosts)
            .map(|h| {
                [
                    self.stage_cost(DeployStage::Eval, h),
                    self.stage_cost(DeployStage::Health, h),
                    self.stage_cost(DeployStage::Build, h),
                    self.stage_cost(DeployStage::Copy, h),
                    self.stage_cost(DeployStage::Activate, h),
                ]
            })
            .collect()
    }

    /// Run the deterministic virtual schedule.
    ///
    /// Stages are walked in pipeline order, hosts in index order. For
    /// each task the scheduler takes the slot that frees up earliest
    /// and starts at `max(slot_free_at, previous_stage_end)`. That
    /// fixed order plus earliest-free-slot selection is what makes the
    /// output reproducible: no thread, no clock, no `HashMap` iteration
    /// in the decision path.
    pub fn run(&self) -> DagSimReport {
        let costs = self.cost_matrix();
        let n = self.cfg.hosts;

        // end[host][stage] = virtual time that host finished `stage`.
        let mut end: Vec<[Duration; 5]> = vec![[Duration::ZERO; 5]; n];
        let mut stages: Vec<StageTiming> = Vec::with_capacity(5);
        let mut failures: Vec<(DeployStage, usize, String)> = Vec::new();
        let mut dag_end = Duration::ZERO;

        // reached[host] is false once the host's chain has been broken
        // by an injected failure; a broken host costs nothing at any
        // later stage.
        let mut reached: Vec<bool> = vec![true; n];

        for stage in DeployStage::ALL {
            let si = stage.index();
            let limit = self.cfg.schedule.limit(stage).max(1);
            // One free-time per concurrency slot. A stage with limit
            // `k` behaves like a `k`-slot pool.
            let mut slots: Vec<Duration> = vec![Duration::ZERO; limit];
            let mut first_start: Option<Duration> = None;
            let mut last_end = Duration::ZERO;
            let mut total_work = Duration::ZERO;
            let mut converged = 0usize;
            let mut intervals: Vec<(Duration, Duration)> = Vec::with_capacity(n);

            for host in 0..n {
                if !reached[host] {
                    continue;
                }
                let cost = costs[host][si];
                let ready = if si == 0 {
                    Duration::ZERO
                } else {
                    end[host][si - 1]
                };

                // Earliest-free-slot selection: the lowest index among
                // slots with the minimum free time, so ties are stable.
                let mut best = 0usize;
                for k in 1..slots.len() {
                    if slots[k] < slots[best] {
                        best = k;
                    }
                }
                let start = ready.max(slots[best]);
                let stop = start + cost;
                slots[best] = stop;
                end[host][si] = stop;
                total_work += cost;
                intervals.push((start, stop));
                if first_start.is_none_or(|s| start < s) {
                    first_start = Some(start);
                }
                if stop > last_end {
                    last_end = stop;
                }
            }

            // A host converges at this stage iff its chain is intact
            // and it was not failed here. A failure truncates the chain
            // for every later stage.
            for host in 0..n {
                if !reached[host] {
                    continue;
                }
                match self.cfg.failures.check(stage, host) {
                    Some(reason) => {
                        failures.push((stage, host, reason.to_string()));
                        reached[host] = false;
                    }
                    None => converged += 1,
                }
            }

            let makespan = last_end.saturating_sub(first_start.unwrap_or(Duration::ZERO));
            let peak = peak_concurrency(&intervals);
            let efficiency = if makespan.is_zero() || peak == 0 {
                1.0
            } else {
                (total_work.as_secs_f64() / (makespan.as_secs_f64() * peak as f64)).clamp(0.0, 1.0)
            };
            stages.push(StageTiming {
                stage,
                limit,
                makespan,
                total_work,
                peak_concurrency: peak,
                converged_hosts: converged,
                parallel_efficiency: efficiency,
            });
            if last_end > dag_end {
                dag_end = last_end;
            }
        }

        let converged_hosts = stages
            .iter()
            .find(|s| s.stage == DeployStage::Activate)
            .map(|s| s.converged_hosts)
            .unwrap_or(0);

        DagSimReport {
            hosts: n,
            stages,
            wall_time: dag_end,
            converged_hosts,
            failures,
        }
    }

    /// Build and run the **real** DAG through
    /// [`consortium::dag::DagExecutor`].
    ///
    /// Tasks carry the same costs [`Self::run`] schedules, and each one
    /// asserts its predecessor's output exists before succeeding — so a
    /// real ordering violation surfaces as a task failure, not as a
    /// silently-passing test. Injected failures use
    /// [`ErrorPolicy::ContinueIndependent`], the same policy
    /// `consortium_nix::deploy` uses, so downstream stages cancel and
    /// unrelated hosts keep going.
    pub fn validate(&self) -> DagReport {
        let ctx = DagContext::new();
        let costs: Arc<HashMap<String, [Duration; 5]>> = Arc::new((0..self.cfg.hosts)
            .map(|h| {
                let c = [
                    self.stage_cost(DeployStage::Eval, h),
                    self.stage_cost(DeployStage::Health, h),
                    self.stage_cost(DeployStage::Build, h),
                    self.stage_cost(DeployStage::Copy, h),
                    self.stage_cost(DeployStage::Activate, h),
                ];
                (self.cfg.host_name(h), c)
            })
            .collect());
        let failures = self.cfg.failures.clone();
        let hosts: Vec<String> = (0..self.cfg.hosts).map(|h| self.cfg.host_name(h)).collect();

        let mut builder = StageBuilder::new()
            .resources(hosts)
            .error_policy(ErrorPolicy::ContinueIndependent)
            .context(ctx);

        for stage in DeployStage::ALL {
            let si = stage.index();
            let limit = self.cfg.schedule.limit(stage);
            let failures = failures.clone();
            let costs = Arc::clone(&costs);
            builder = builder.stage(
                stage.name(),
                Some(limit),
                move |host: &str| {
                    let cost = costs.get(host).map(|c| c[si]).unwrap_or(Duration::ZERO);
                    let prev = if si == 0 {
                        None
                    } else {
                        Some(TaskId(format!(
                            "{}:{}",
                            DeployStage::ALL[si - 1].name(),
                            host
                        )))
                    };
                    let host_idx: usize = host
                        .rsplit('-')
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let fail = failures.check(stage, host_idx).map(str::to_string);
                    Box::new(SimStageTask {
                        task_id: TaskId(format!("{}:{}", stage.name(), host)),
                        prev,
                        cost,
                        failure: fail,
                    }) as Box<dyn DagTask>
                },
            );
        }

        builder
            .build()
            .expect("deploy DAG must build")
            .run()
            .expect("deploy DAG must run")
    }
}

/// Peak simultaneous in-flight tasks, by sweeping interval endpoints.
///
/// A start at the same instant another ends does not count as overlap,
/// so ends are processed before starts at equal times.
fn peak_concurrency(intervals: &[(Duration, Duration)]) -> usize {
    let mut events: Vec<(Duration, i8)> = Vec::with_capacity(intervals.len() * 2);
    for (s, e) in intervals {
        events.push((*s, 1));
        events.push((*e, -1));
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut cur = 0i64;
    let mut peak = 0usize;
    for (_, delta) in events {
        cur += delta as i64;
        if cur > peak as i64 {
            peak = cur as usize;
        }
    }
    peak
}

/// A synthetic [`DagTask`] with no I/O.
struct SimStageTask {
    task_id: TaskId,
    prev: Option<TaskId>,
    cost: Duration,
    failure: Option<String>,
}

impl DagTask for SimStageTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        if let Some(prev) = &self.prev {
            if !ctx.has_output(prev) {
                return TaskOutcome::Failed(format!(
                    "ordering violated: {} started before {} finished",
                    self.task_id, prev
                ));
            }
        }
        if let Some(reason) = &self.failure {
            return TaskOutcome::Failed(reason.clone());
        }
        // Store the cost as the stage output; dependents only need its
        // presence, but keeping it non-zero makes an accidental no-op
        // visible in debugging.
        ctx.set_output(self.task_id.clone(), self.cost);
        TaskOutcome::Success
    }

    fn describe(&self) -> String {
        format!("sim deploy stage {}", self.task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{LinkDirection, PacketLoss};

    fn cfg(hosts: usize) -> DagSimConfig {
        DagSimConfig {
            seed: 0xc0ffee,
            hosts,
            closure_bytes: 100 * 1024 * 1024,
            ..Default::default()
        }
    }

    #[test]
    fn stage_names_round_trip() {
        for s in DeployStage::ALL {
            assert_eq!(DeployStage::from_name(s.name()), Some(s));
        }
        assert_eq!(DeployStage::from_name("nope"), None);
    }

    #[test]
    fn production_schedule_matches_deploy_limits() {
        let s = StageSchedule::production(8);
        assert_eq!(s.limit(DeployStage::Eval), 1, "eval is serial");
        assert_eq!(s.limit(DeployStage::Build), 8);
        assert_eq!(s.limit(DeployStage::Activate), 4, "activate caps at 4");
        assert_eq!(StageSchedule::production(2).limit(DeployStage::Activate), 2);
    }

    #[test]
    fn run_is_deterministic_for_a_seed() {
        let sim = DeployDagSim::new(cfg(32));
        let a = sim.run();
        let b = DeployDagSim::new(cfg(32)).run();
        assert_eq!(a, b, "same seed produced different reports");
    }

    #[test]
    fn different_seeds_change_costs_but_not_convergence() {
        let mut c1 = cfg(32);
        c1.seed = 1;
        let mut c2 = cfg(32);
        c2.seed = 2;
        let a = DeployDagSim::new(c1).run();
        let b = DeployDagSim::new(c2).run();
        assert_eq!(a.converged_hosts, b.converged_hosts);
        assert_ne!(
            a.wall_time, b.wall_time,
            "host variation should make seeds differ"
        );
    }

    #[test]
    fn clean_run_converges_every_host_in_every_stage() {
        let r = DeployDagSim::new(cfg(64)).run();
        assert!(r.is_success(), "clean run reported {:?}", r.failures);
        assert_eq!(r.converged_hosts, 64);
        for s in DeployStage::ALL {
            assert_eq!(r.converged_at(s), 64, "{} short", s.name());
        }
    }

    #[test]
    fn serial_eval_stage_is_the_bottleneck_at_scale() {
        let r = DeployDagSim::new(cfg(64)).run();
        let eval = r.stage(DeployStage::Eval);
        // eval_limit = 1, so peak concurrency must be 1 and efficiency 1.0.
        assert_eq!(eval.peak_concurrency, 1);
        assert!((eval.parallel_efficiency - 1.0).abs() < 1e-9);
        assert!(eval.makespan >= eval.total_work);
    }

    #[test]
    fn wider_parallelism_shortens_the_copy_stage() {
        let base = cfg(64);
        let narrow = DeployDagSim::new(DagSimConfig {
            schedule: StageSchedule::production(1),
            ..base.clone()
        })
        .run();
        let wide = DeployDagSim::new(DagSimConfig {
            schedule: StageSchedule::production(32),
            ..base
        })
        .run();
        assert!(
            wide.stage(DeployStage::Copy).makespan
                < narrow.stage(DeployStage::Copy).makespan,
            "copy did not speed up with more parallelism"
        );
    }

    #[test]
    fn peak_concurrency_never_exceeds_the_limit() {
        for limit in [1usize, 3, 8, 32] {
            let r = DeployDagSim::new(DagSimConfig {
                hosts: 48,
                schedule: StageSchedule::production(limit),
                ..cfg(48)
            })
            .run();
            for t in &r.stages {
                assert!(
                    t.peak_concurrency <= t.limit,
                    "{} peak {} > limit {}",
                    t.stage.name(),
                    t.peak_concurrency,
                    t.limit
                );
            }
        }
    }

    #[test]
    fn copy_cost_dominates_when_the_closure_is_large() {
        let r = DeployDagSim::new(cfg(16)).run();
        let copy = r.stage(DeployStage::Copy);
        let build = r.stage(DeployStage::Build);
        assert!(
            copy.total_work > build.total_work,
            "100 MB closure over the fabric should cost more than an 8 MB build"
        );
    }

    #[test]
    fn packet_loss_slows_the_copy_stage() {
        let clean = cfg(32);
        let mut lossy = clean.clone();
        lossy.link = LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.01))
            .with_direction(LinkDirection::UplinkLimited);
        let a = DeployDagSim::new(clean).run();
        let b = DeployDagSim::new(lossy).run();
        assert!(
            b.stage(DeployStage::Copy).total_work > a.stage(DeployStage::Copy).total_work,
            "loss did not increase copy cost"
        );
        assert_eq!(a.converged_hosts, b.converged_hosts);
    }

    #[test]
    fn asymmetric_uplinks_show_up_in_copy_cost() {
        let mut c = cfg(32);
        c.network.uplinks = Some(UplinkDistribution::Uniform(10 * 1024 * 1024));
        let contended = DeployDagSim::new(c).run();
        let plain = DeployDagSim::new(cfg(32)).run();
        assert!(
            contended.stage(DeployStage::Copy).total_work
                > plain.stage(DeployStage::Copy).total_work
        );
    }

    #[test]
    fn injected_failure_truncates_the_chain_for_that_host() {
        let mut c = cfg(16);
        c.failures = DagFailures::Hosts {
            stage: DeployStage::Build,
            hosts: [3].into_iter().collect(),
            reason: "builder OOM".to_string(),
        };
        let r = DeployDagSim::new(c).run();
        assert_eq!(r.converged_at(DeployStage::Build), 15);
        assert_eq!(r.converged_at(DeployStage::Copy), 15);
        assert_eq!(r.converged_at(DeployStage::Activate), 15);
        assert_eq!(r.converged_hosts, 15);
        assert!(!r.is_success());
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].0, DeployStage::Build);
        assert_eq!(r.failures[0].1, 3);
    }

    #[test]
    fn every_nth_host_failure_scales() {
        let mut c = cfg(64);
        c.failures = DagFailures::EveryNthHost {
            stage: DeployStage::Eval,
            every: 8,
            reason: "eval blew up".to_string(),
        };
        let r = DeployDagSim::new(c).run();
        assert_eq!(r.failures.len(), 8, "64 hosts / every 8th");
        assert_eq!(r.converged_hosts, 56);
    }

    #[test]
    fn failures_never_reach_a_downstream_stage() {
        let mut c = cfg(32);
        c.failures = DagFailures::EveryNthHost {
            stage: DeployStage::Copy,
            every: 4,
            reason: "link down".to_string(),
        };
        let r = DeployDagSim::new(c).run();
        assert_eq!(r.converged_at(DeployStage::Build), 32, "build is upstream");
        assert_eq!(r.converged_at(DeployStage::Copy), 24);
        assert_eq!(r.converged_at(DeployStage::Activate), 24);
    }

    #[test]
    fn validate_agrees_with_run_on_a_clean_graph() {
        let sim = DeployDagSim::new(cfg(24));
        let r = sim.run();
        let d = sim.validate();
        assert!(d.is_success(), "real DAG failed: {:?}", d.failed);
        assert_eq!(d.completed.len(), 24 * DeployStage::ALL.len());
        assert_eq!(d.cancelled.len(), 0);
        assert_eq!(d.completed.len(), 24 * 5);
        assert_eq!(r.converged_hosts, 24);
    }

    #[test]
    fn validate_agrees_with_run_under_injected_failures() {
        let mut c = cfg(24);
        c.failures = DagFailures::EveryNthHost {
            stage: DeployStage::Build,
            every: 6,
            reason: "nix build failed".to_string(),
        };
        let sim = DeployDagSim::new(c);
        let r = sim.run();
        let d = sim.validate();

        // Hosts 0/6/12/18 fail at build: 4 failures, and their copy +
        // activate are cancelled downstream.
        assert_eq!(d.failed.len(), 4, "failed: {:?}", d.failed);
        assert_eq!(d.cancelled.len(), 8, "cancelled: {:?}", d.cancelled);

        // The 4 broken hosts still completed the two *upstream* stages,
        // so total completed is 20*5 + 4*2 = 108, not 100.
        assert_eq!(d.completed.len(), 108);
        let completed_in = |stage: DeployStage| {
            let p = format!("{}:", stage.name());
            d.completed.iter().filter(|id| id.0.starts_with(&p)).count()
        };
        // The invariant that matters: the final stage only ever
        // completes for hosts whose whole chain succeeded. No host is
        // left half-activated.
        assert_eq!(completed_in(DeployStage::Eval), 24);
        assert_eq!(completed_in(DeployStage::Build), 20);
        assert_eq!(completed_in(DeployStage::Activate), 20);
        assert_eq!(r.converged_hosts, 20, "sim and executor must agree");
        assert_eq!(completed_in(DeployStage::Activate), r.converged_hosts);
    }

    #[test]
    fn validate_never_reports_an_ordering_violation() {
        for limit in [1usize, 4, 16] {
            let sim = DeployDagSim::new(DagSimConfig {
                hosts: 32,
                schedule: StageSchedule::production(limit),
                ..cfg(32)
            });
            let d = sim.validate();
            for (id, err) in &d.failed {
                assert!(
                    !err.contains("ordering violated"),
                    "{} failed on ordering: {}",
                    id,
                    err
                );
            }
        }
    }

    #[test]
    fn zero_hosts_is_reported_as_empty_not_a_panic() {
        let r = DeployDagSim::new(cfg(0)).run();
        assert_eq!(r.hosts, 0);
        assert_eq!(r.stages.len(), 5);
        assert_eq!(r.wall_time, Duration::ZERO);
    }

    #[test]
    fn host_variation_of_zero_makes_costs_identical_across_hosts() {
        let mut c = cfg(16);
        c.host_variation = 0.0;
        c.network.uplinks = None;
        let sim = DeployDagSim::new(c);
        let a = sim.stage_cost(DeployStage::Build, 0);
        for h in 1..16 {
            assert_eq!(a, sim.stage_cost(DeployStage::Build, h));
        }
    }
}
