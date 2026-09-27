//! Cascade-strategy microbench — the canonical perf signal for the
//! `perf-cascade-strategy` autoresearch task type.
//!
//! Three scenarios per strategy, fixed seeds for determinism:
//!
//! - `uniform/256`: 256 nodes, uniform 100 MB/s edges, 50 MB closure.
//!   Baseline; all strategies should converge in ~⌈log₂(256)⌉ = 8 rounds.
//! - `bimodal/256`: 256 nodes, bimodal bandwidth (1 MB/s slow vs 1 GB/s
//!   fast at 30%-fast), 50 MB closure. Where MaxBottleneckSpanning
//!   should beat Log2FanOut.
//! - `bimodal/512`: 512 nodes, same bandwidth shape, larger N to surface
//!   strategy-quality differences at scale.
//!
//! Result: criterion writes `target/criterion/<bench>/new/estimates.json`.
//! The autoresearch score gate reads `.mean.point_estimate` from
//! `bimodal/256/<strategy>` per strategy and asserts the agent's edits
//! made it faster.

use consortium_fanout_sim::fixtures::BandwidthDistribution;
use consortium_fanout_sim::fixtures::FailureSchedule;
use consortium_fanout_sim::scenario::{Scenario, ScenarioConfig};
use consortium_nix::cascade::{CascadeStrategy, Log2FanOut};
use consortium_nix::cascade_strategies::{MaxBottleneckSpanning, SteinerGreedy};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

/// Deliberate slowdown used by the perf-gate acceptance test.
///
/// Reads `CONSORTIUM_PERF_REGRESSION_PCT` (default `0`, i.e. off) and burns
/// that percentage of the measured wall time in a black-boxed spin loop. It is
/// *self-calibrating* — the delay scales with how long the scenario actually
/// took — so a "10% regression" is 10% on any host, CI runner included, rather
/// than a fixed number of iterations that would mean something different on
/// every machine.
///
/// Unset in normal bench runs, so the shipped signal is unmodified; see
/// `autoresearch/scripts/test-perf-gate-e2e.sh`.
fn burn_regression_pct(elapsed: std::time::Duration) {
    let pct: f64 = std::env::var("CONSORTIUM_PERF_REGRESSION_PCT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    if pct <= 0.0 {
        return;
    }
    let target = elapsed.as_secs_f64() * pct / 100.0;
    let start = std::time::Instant::now();
    let mut acc = std::hint::black_box(0u64);
    while start.elapsed().as_secs_f64() < target {
        for i in 0..256u64 {
            acc = acc.wrapping_add(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
    }
    std::hint::black_box(acc);
}

fn run_strategy(cfg: &ScenarioConfig, strategy: &dyn CascadeStrategy) -> u32 {
    let start = std::time::Instant::now();
    let r = Scenario::new(cfg.clone()).run(strategy);
    burn_regression_pct(start.elapsed());
    // Touch result so the optimizer can't elide the run.
    std::hint::black_box(r.converged.len() as u32 + r.rounds)
}

fn bench_uniform_256(c: &mut Criterion) {
    let cfg = ScenarioConfig {
        seed: 0xC0FFEE,
        n_nodes: 256,
        seed_fraction: 0.0,
        closure_bytes: 50 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Uniform(100 * 1024 * 1024),
        uplinks: None,
        failures: FailureSchedule::None,
        max_rounds: 32,
    };
    let mut g = c.benchmark_group("cascade_strategies/uniform/256");
    g.measurement_time(std::time::Duration::from_secs(3));
    for (name, strategy) in strategies() {
        g.bench_with_input(BenchmarkId::from_parameter(name), &cfg, |b, cfg| {
            b.iter(|| run_strategy(cfg, strategy));
        });
    }
    g.finish();
}

fn bench_bimodal_256(c: &mut Criterion) {
    let cfg = ScenarioConfig {
        seed: 0xBADBEEF,
        n_nodes: 256,
        seed_fraction: 0.0,
        closure_bytes: 50 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 1024 * 1024,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: None,
        failures: FailureSchedule::None,
        max_rounds: 32,
    };
    let mut g = c.benchmark_group("cascade_strategies/bimodal/256");
    g.measurement_time(std::time::Duration::from_secs(3));
    for (name, strategy) in strategies() {
        g.bench_with_input(BenchmarkId::from_parameter(name), &cfg, |b, cfg| {
            b.iter(|| run_strategy(cfg, strategy));
        });
    }
    g.finish();
}

fn bench_bimodal_512(c: &mut Criterion) {
    let cfg = ScenarioConfig {
        seed: 0xDEADBEEF,
        n_nodes: 512,
        seed_fraction: 0.0,
        closure_bytes: 50 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 1024 * 1024,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: None,
        failures: FailureSchedule::None,
        max_rounds: 32,
    };
    let mut g = c.benchmark_group("cascade_strategies/bimodal/512");
    g.measurement_time(std::time::Duration::from_secs(3));
    for (name, strategy) in strategies() {
        g.bench_with_input(BenchmarkId::from_parameter(name), &cfg, |b, cfg| {
            b.iter(|| run_strategy(cfg, strategy));
        });
    }
    g.finish();
}

fn strategies() -> Vec<(&'static str, &'static dyn CascadeStrategy)> {
    vec![
        ("log2-fanout", &Log2FanOut),
        ("max-bottleneck-spanning", &MaxBottleneckSpanning),
        ("steiner-greedy", &SteinerGreedy),
    ]
}

criterion_group!(
    benches,
    bench_uniform_256,
    bench_bimodal_256,
    bench_bimodal_512
);
criterion_main!(benches);
