//! High-level scenario builder.
//!
//! Wraps the cascade primitive + executor + fixtures into a single
//! reproducible test case. Pass it a [`ScenarioConfig`] and a strategy;
//! get back a [`CascadeResult`].

use consortium_nix::cascade::{
    run_cascade, CascadeNode, CascadeResult, CascadeStrategy, NetworkProfile, NodeIdAlloc,
};
use consortium_nix::cascade_trace::{CascadeTrace, TraceRecorder};

use crate::executor::DeterministicExecutor;
use crate::fixtures::{
    rng_from_seed, BandwidthDistribution, FailureSchedule, SeedDistribution, UplinkDistribution,
};
use crate::link::LinkModel;

/// Everything needed to build a reproducible cascade run.
///
/// # Example
///
/// ```ignore
/// let cfg = ScenarioConfig {
///     seed: 42,
///     n_nodes: 32,
///     bandwidth: BandwidthDistribution::Uniform(100 * 1024 * 1024),
///     uplinks: Some(UplinkDistribution::Bimodal {
///         slow: 10 * 1024 * 1024,
///         fast: 1024 * 1024 * 1024,
///         fast_fraction: 0.3,
///     }),
///     ..ScenarioConfig::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub seed: u64,
    pub n_nodes: u32,
    pub seed_fraction: f64,
    pub closure_bytes: u64,
    pub bandwidth: BandwidthDistribution,
    /// Optional per-node link capacities. When `Some`, contention math is
    /// engaged via [`NetworkProfile::effective_bandwidth`]. When `None`,
    /// the executor falls back to plain per-edge bandwidth (same as Phase 1
    /// behavior).
    pub uplinks: Option<UplinkDistribution>,
    pub failures: FailureSchedule,
    pub max_rounds: u32,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            n_nodes: 16,
            seed_fraction: 0.0,
            closure_bytes: 100 * 1024 * 1024,
            bandwidth: BandwidthDistribution::Uniform(100 * 1024 * 1024),
            uplinks: None,
            failures: FailureSchedule::None,
            max_rounds: 64,
        }
    }
}

pub struct Scenario {
    cfg: ScenarioConfig,
}

impl Scenario {
    pub fn new(cfg: ScenarioConfig) -> Self {
        Self { cfg }
    }

    /// Run the cascade with the given strategy. Deterministic in `cfg.seed`.
    pub fn run(&self, strategy: &dyn CascadeStrategy) -> CascadeResult {
        self.run_with_link(strategy, LinkModel::new())
    }

    /// Run the cascade, collecting a per-round trace into `rec`.
    ///
    /// Same result as [`Self::run`] — the sink is observational only — but
    /// the recorder retains the per-edge outcomes and parent chain that
    /// [`crate::report::RunReport::from_cascade`] needs to derive
    /// fan-out depth, per-edge throughput and convergence times.
    pub fn run_traced(
        &self,
        strategy: &dyn CascadeStrategy,
        rec: &TraceRecorder,
    ) -> (CascadeResult, CascadeTrace) {
        self.dispatch(strategy, LinkModel::new(), Some(rec))
    }

    /// [`Self::run_traced`] with a custom [`LinkModel`].
    pub fn run_with_link_traced(
        &self,
        strategy: &dyn CascadeStrategy,
        link: LinkModel,
        rec: &TraceRecorder,
    ) -> (CascadeResult, CascadeTrace) {
        self.dispatch(strategy, link, Some(rec))
    }

    /// Run the cascade with a custom [`LinkModel`].
    ///
    /// [`Self::run`] is exactly this with a lossless, zero-jitter
    /// model. Pass a model with [`PacketLoss`] and jitter to price a
    /// lossy, noisy fabric. The link model only affects edge
    /// *durations*; failure decisions still come from
    /// `cfg.failures` and `NetworkProfile::partitions`.
    pub fn run_with_link(&self, strategy: &dyn CascadeStrategy, link: LinkModel) -> CascadeResult {
        self.dispatch(strategy, link, None).0
    }

    fn dispatch(
        &self,
        strategy: &dyn CascadeStrategy,
        link: LinkModel,
        rec: Option<&TraceRecorder>,
    ) -> (CascadeResult, CascadeTrace) {
        let mut rng = rng_from_seed(self.cfg.seed);

        let mut alloc = NodeIdAlloc::new();
        let nodes: Vec<CascadeNode> = (0..self.cfg.n_nodes)
            .map(|_| {
                let id = alloc.alloc();
                CascadeNode::new(id, format!("user@host-{}", id.0))
            })
            .collect();

        let seeded = if self.cfg.seed_fraction > 0.0 {
            SeedDistribution::Random {
                fraction: self.cfg.seed_fraction,
            }
            .sample(&mut rng, self.cfg.n_nodes)
        } else {
            SeedDistribution::Single.sample(&mut rng, self.cfg.n_nodes)
        };

        let mut net = NetworkProfile::default();
        self.cfg
            .bandwidth
            .populate(&mut rng, &mut net, self.cfg.n_nodes);
        if let Some(uplinks) = &self.cfg.uplinks {
            uplinks.populate(&mut rng, &mut net, self.cfg.n_nodes);
        }

        let exec = DeterministicExecutor::new(self.cfg.closure_bytes, self.cfg.failures.clone())
            .with_link_model(link)
            .with_seed(self.cfg.seed);

        let result = run_cascade(
            nodes,
            seeded,
            net,
            strategy,
            &exec,
            self.cfg.max_rounds,
            rec.map(|r| r as &dyn consortium_nix::cascade::TraceSink),
        );

        let trace = CascadeTrace::from_recorder(
            strategy.name(),
            self.cfg.n_nodes,
            rec.unwrap_or_else(|| {
                // A run without a sink still needs a trace object to hand
                // back; an empty recorder yields an empty, valid trace.
                static EMPTY: std::sync::OnceLock<TraceRecorder> = std::sync::OnceLock::new();
                EMPTY.get_or_init(TraceRecorder::new)
            }),
        );

        (result, trace)
    }

    pub fn config(&self) -> &ScenarioConfig {
        &self.cfg
    }
}

/// Compare strategies on the same scenario seed. Returns
/// `(strategy_name, result)` pairs in input order.
pub fn compare_strategies<'a>(
    cfg: &ScenarioConfig,
    strategies: &'a [&'a dyn CascadeStrategy],
) -> Vec<(&'a str, CascadeResult)> {
    strategies
        .iter()
        .map(|s| (s.name(), Scenario::new(cfg.clone()).run(*s)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use consortium_nix::cascade::Log2FanOut;
    use consortium_nix::cascade_strategies::{MaxBottleneckSpanning, SteinerGreedy};

    #[test]
    fn scenario_is_reproducible_from_seed() {
        let cfg = ScenarioConfig {
            seed: 0xc0ffee,
            n_nodes: 32,
            seed_fraction: 0.0,
            closure_bytes: 50 * 1024 * 1024,
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 10 * 1024 * 1024,
                fast: 1024 * 1024 * 1024,
                fast_fraction: 0.3,
            },
            uplinks: None,
            failures: FailureSchedule::None,
            max_rounds: 32,
        };
        let r1 = Scenario::new(cfg.clone()).run(&Log2FanOut);
        let r2 = Scenario::new(cfg).run(&Log2FanOut);
        assert_eq!(r1.rounds, r2.rounds);
        assert_eq!(r1.converged.len(), r2.converged.len());
        assert_eq!(r1.round_durations, r2.round_durations);
    }

    #[test]
    fn comparison_across_strategies_runs_clean() {
        let cfg = ScenarioConfig {
            seed: 1,
            n_nodes: 64,
            seed_fraction: 0.0,
            closure_bytes: 50 * 1024 * 1024,
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 10 * 1024 * 1024,
                fast: 1024 * 1024 * 1024,
                fast_fraction: 0.5,
            },
            uplinks: None,
            failures: FailureSchedule::None,
            max_rounds: 32,
        };
        let strategies: Vec<&dyn CascadeStrategy> =
            vec![&Log2FanOut, &MaxBottleneckSpanning, &SteinerGreedy];
        let results = compare_strategies(&cfg, &strategies);
        assert_eq!(results.len(), 3);
        for (name, result) in &results {
            assert!(result.is_success(), "{} failed: {:?}", name, result.failed);
            assert_eq!(result.converged.len(), 64);
        }
    }
}
