//! Emit per-run artifacts in the CON-20 report schema.
//!
//! The testing-requirements rubric (CON-15) needs reference scenarios, and
//! the report pipeline (CON-20) needs machine-readable per-run metrics.
//! This example is the bridge: it runs the cascade and the deploy DAG at
//! each scale under a fixed seed, and writes one `<scenario>.json` plus one
//! `<scenario>.md` per run.
//!
//! Run:
//!   cargo run --release --example emit_report -p consortium-fanout-sim
//!   cargo run --release --example emit_report -p consortium-fanout-sim -- --out target/reports
//!
//! Output is byte-stable for a given seed, so a checked-in directory of
//! these files works directly as a regression baseline for CON-20's
//! baseline-comparison step.

use std::path::PathBuf;

use consortium_fanout_sim::dag_sim::{
    DagFailures, DagNetwork, DagSimConfig, DeployDagSim, StageSchedule,
};
use consortium_fanout_sim::fixtures::{BandwidthDistribution, FailureSchedule};
use consortium_fanout_sim::link::{LinkDirection, LinkModel, PacketLoss};
use consortium_fanout_sim::report::RunReport;
use consortium_fanout_sim::scenario::{Scenario as SimScenario, ScenarioConfig};
use consortium_nix::cascade::{CascadeStrategy, Log2FanOut, NodeId};
use consortium_nix::cascade_trace::TraceRecorder;

/// The scales the harness is specified against.
const SCALES: [u32; 3] = [64, 256, 1024];

/// Fixed so re-running produces byte-identical artifacts.
const SEED: u64 = 0x5eed_0000;

fn main() {
    let out = parse_out_dir();
    if let Err(e) = std::fs::create_dir_all(&out) {
        eprintln!("cannot create {}: {e}", out.display());
        std::process::exit(1);
    }

    println!("writing reports to {}\n", out.display());
    println!(
        "{:<26} {:>6} {:>7} {:>10} {:>12} {:>10}",
        "scenario", "hosts", "rounds", "depth", "converge", "min MiB/s"
    );
    println!("{}", "-".repeat(76));

    for n in SCALES {
        for scenario in scenarios() {
            let report = run(scenario.n_override.unwrap_or(n), &scenario);
            let base = format!("{}-n{}", scenario.name, n);

            let json_path = out.join(format!("{base}.json"));
            let md_path = out.join(format!("{base}.md"));
            if let Err(e) = std::fs::write(&json_path, report.to_json()) {
                eprintln!("write {}: {e}", json_path.display());
                std::process::exit(1);
            }
            if let Err(e) = std::fs::write(&md_path, report.to_markdown()) {
                eprintln!("write {}: {e}", md_path.display());
                std::process::exit(1);
            }

            println!(
                "{:<26} {:>6} {:>7} {:>10} {:>11.2}s {:>10.1}",
                scenario.name,
                report.converged_hosts(),
                report.rounds,
                report.fan_out_depth,
                report
                    .time_to_converge
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(f64::NAN),
                report.min_goodput().unwrap_or(0.0) / (1024.0 * 1024.0),
            );
        }
    }

    println!(
        "\nwrote {} artifact pairs",
        SCALES.len() * scenarios().len()
    );
}

struct ReportScenario {
    name: &'static str,
    failures: FailureSchedule,
    lossy: bool,
    n_override: Option<u32>,
}

/// A realistic-but-not-pathological fleet: mixed link speeds, a lossy and
/// noisy fabric, and an uplink that is the real bottleneck on copy.
fn scenarios() -> Vec<ReportScenario> {
    vec![
        ReportScenario {
            name: "clean",
            failures: FailureSchedule::None,
            lossy: false,
            n_override: None,
        },
        ReportScenario {
            name: "lossy",
            failures: FailureSchedule::None,
            lossy: true,
            n_override: None,
        },
        ReportScenario {
            name: "one-dead-node",
            failures: FailureSchedule::KillNodeAtRound {
                node: NodeId(3),
                round: 1,
            },
            lossy: false,
            n_override: None,
        },
        // Scenarios that only make sense at one size: the whole fleet is
        // the failure, and a single seed has nothing to fan out to.
        ReportScenario {
            name: "single-host",
            failures: FailureSchedule::None,
            lossy: false,
            n_override: Some(1),
        },
    ]
}

fn run(n: u32, scenario: &ReportScenario) -> RunReport {
    let cfg = ScenarioConfig {
        seed: SEED ^ (n as u64),
        n_nodes: n,
        seed_fraction: 0.0,
        closure_bytes: 100 * 1024 * 1024,
        bandwidth: BandwidthDistribution::Bimodal {
            slow: 10 * 1024 * 1024,
            fast: 1024 * 1024 * 1024,
            fast_fraction: 0.3,
        },
        uplinks: Some(
            consortium_fanout_sim::fixtures::UplinkDistribution::Bimodal {
                slow: 20 * 1024 * 1024,
                fast: 512 * 1024 * 1024,
                fast_fraction: 0.4,
            },
        ),
        failures: scenario.failures.clone(),
        max_rounds: 64,
    };

    let link = if scenario.lossy {
        LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.03))
            .with_jitter(0.2)
            .with_direction(LinkDirection::UplinkLimited)
    } else {
        LinkModel::new()
    };

    let rec = TraceRecorder::new();
    let (result, trace) =
        SimScenario::new(cfg.clone()).run_with_link_traced(&Log2FanOut, link, &rec);

    let report = RunReport::from_cascade(
        RunReport::meta(&cfg, Log2FanOut.name()),
        &result,
        &trace,
        cfg.closure_bytes,
    );

    // Attach the deploy-DAG phase timings from the same host count, so one
    // report answers both "how did the cascade fan out" and "where did the
    // deploy spend its time".
    report.with_dag(&deploy_dag(n as usize))
}

fn deploy_dag(hosts: usize) -> consortium_fanout_sim::dag_sim::DagSimReport {
    DeployDagSim::new(DagSimConfig {
        seed: SEED ^ (hosts as u64),
        hosts,
        closure_bytes: 100 * 1024 * 1024,
        builder_bandwidth: 2 * 1024 * 1024 * 1024,
        schedule: StageSchedule::production(16),
        cost: consortium_fanout_sim::dag_sim::StageCost::default(),
        network: DagNetwork {
            bandwidth: BandwidthDistribution::Bimodal {
                slow: 10 * 1024 * 1024,
                fast: 1024 * 1024 * 1024,
                fast_fraction: 0.3,
            },
            uplinks: Some(
                consortium_fanout_sim::fixtures::UplinkDistribution::Bimodal {
                    slow: 20 * 1024 * 1024,
                    fast: 512 * 1024 * 1024,
                    fast_fraction: 0.4,
                },
            ),
            latency: std::time::Duration::from_millis(2),
        },
        link: LinkModel::new()
            .with_loss(PacketLoss::PerChunk(0.03))
            .with_jitter(0.2)
            .with_direction(LinkDirection::UplinkLimited),
        host_variation: 0.3,
        failures: DagFailures::None,
    })
    .run()
}

fn parse_out_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--out" {
            if let Some(v) = args.next() {
                return PathBuf::from(v);
            }
        }
    }
    PathBuf::from("target/reports")
}
