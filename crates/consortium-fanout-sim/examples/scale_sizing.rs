//! Scale-ceiling sizing harness for the testing-requirements rubric.
//!
//! Answers one question the rubric must not leave as hand-waving: **at what
//! N does the deterministic sim stop being a cheap CI citizen?** It sweeps
//! N, measures process wall-time and peak RSS, and prints the *simulated*
//! time-to-converge (sum of `round_durations`) separately from wall-time —
//! the two are unrelated quantities and conflating them is how "the sim is
//! fast" conclusions go wrong.
//!
//! Run:
//!   cargo run --release --example scale_sizing -p consortium-fanout-sim
//!   cargo run --release --example scale_sizing -p consortium-fanout-sim -- 256 1024 4096
//!
//! Each N is run as its own process by default (`--isolate`) so peak RSS is
//! attributable; pass `--in-process` to run them all in one process instead.
//!
//! This is a *sizing* tool, not a test. It prints a table; it asserts
//! nothing. Thresholds live in `docs/testing-requirements.md`.

use std::collections::HashSet;
use std::time::Instant;

use consortium_fanout_sim::{
    fixtures::{rng_from_seed, BandwidthDistribution, FailureSchedule, UplinkDistribution},
    DeterministicExecutor,
};
use consortium_nix::cascade::{Cascade, CascadeNode, NetworkProfile, NodeId, NodeIdAlloc};
use consortium_nix::cascade_strategies::MaxBottleneckSpanning;

struct Row {
    n: u32,
    rounds: u32,
    converged: u32,
    sim_seconds: f64,
    wall_ms: f64,
    peak_rss_mb: f64,
}

fn run_one(n: u32, seed: u64, bandwidth: BandwidthDistribution) -> Row {
    let t0 = Instant::now();

    let mut rng = rng_from_seed(seed);
    let mut alloc = NodeIdAlloc::new();
    let nodes: Vec<CascadeNode> = (0..n)
        .map(|_| {
            let id = alloc.alloc();
            CascadeNode::new(id, format!("user@host-{}", id.0))
        })
        .collect();

    let mut seeded = HashSet::new();
    seeded.insert(NodeId(0));

    let mut net = NetworkProfile::default();
    bandwidth.populate(&mut rng, &mut net, n);
    UplinkDistribution::Bimodal {
        slow: 1024 * 1024,
        fast: 1024 * 1024 * 1024,
        fast_fraction: 0.3,
    }
    .populate(&mut rng, &mut net, n);

    let exec = DeterministicExecutor::new(100 * 1024 * 1024, FailureSchedule::None);

    let result = Cascade::new()
        .nodes(nodes)
        .seeded(seeded)
        .network(net)
        .strategy(&MaxBottleneckSpanning)
        .executor(&exec)
        .max_rounds(64)
        .run();

    let wall_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let sim_seconds: f64 = result.round_durations.iter().map(|d| d.as_secs_f64()).sum();

    Row {
        n,
        rounds: result.rounds,
        converged: result.converged.len() as u32,
        sim_seconds,
        wall_ms,
        peak_rss_mb: peak_rss_mb(),
    }
}

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v.to_string()))
        })
        .and_then(|kb| kb.parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn print_table(rows: &[Row]) {
    println!(
        "{:>7}  {:>7}  {:>10}  {:>13}  {:>10}  {:>11}  {:>9}",
        "N", "rounds", "converged", "sim-seconds", "wall-ms", "peak-RSS-MB", "ms/node"
    );
    for r in rows {
        println!(
            "{:>7}  {:>7}  {:>10}  {:>13.3}  {:>10.1}  {:>11.1}  {:>9.4}",
            r.n,
            r.rounds,
            r.converged,
            r.sim_seconds,
            r.wall_ms,
            r.peak_rss_mb,
            r.wall_ms / r.n as f64
        );
    }
    println!();
    println!("log2(N) is the convergence lower bound; rounds == ceil(log2(N)) means the");
    println!("strategy is hitting the bound. sim-seconds is *modelled* fleet time, not");
    println!("elapsed test time. wall-ms is what CI actually pays.");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let isolate = !args.iter().any(|a| a == "--in-process");
    let ns: Vec<u32> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .filter_map(|a| a.parse().ok())
        .collect();
    let ns = if ns.is_empty() {
        vec![64, 256, 1024, 2048, 4096, 8192]
    } else {
        ns
    };

    let seed = 0x_dead_beef_u64;
    let bandwidth = || BandwidthDistribution::Uniform(1024 * 1024 * 1024); // 1 GB/s

    println!("# scale-ceiling sizing sweep (uniform 1 GB/s edges, seed {seed:#x})");
    println!("# bandwidth model: Uniform -> NetworkProfile holds N*(N-1) directed edges (O(N^2))");
    println!();

    let mut rows = Vec::new();
    for n in ns {
        if isolate && !rows.is_empty() {
            // Peak RSS is per-process, so report the last child instead of
            // letting a warm heap pollute the next row.
            let self_path = std::env::current_exe().unwrap();
            let out = std::process::Command::new(self_path)
                .arg(n.to_string())
                .arg("--in-process")
                .output()
                .expect("failed to re-exec for isolated measurement");
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("ROW ") {
                    println!("{rest}");
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    rows.push(Row {
                        n: f[0].parse().unwrap(),
                        rounds: f[1].parse().unwrap(),
                        converged: f[2].parse().unwrap(),
                        sim_seconds: f[3].parse().unwrap(),
                        wall_ms: f[4].parse().unwrap(),
                        peak_rss_mb: f[5].parse().unwrap(),
                    });
                }
            }
            continue;
        }
        if isolate {
            let self_path = std::env::current_exe().unwrap();
            let out = std::process::Command::new(self_path)
                .arg(n.to_string())
                .arg("--in-process")
                .output()
                .expect("failed to re-exec for isolated measurement");
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("ROW ") {
                    println!("{rest}");
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    rows.push(Row {
                        n: f[0].parse().unwrap(),
                        rounds: f[1].parse().unwrap(),
                        converged: f[2].parse().unwrap(),
                        sim_seconds: f[3].parse().unwrap(),
                        wall_ms: f[4].parse().unwrap(),
                        peak_rss_mb: f[5].parse().unwrap(),
                    });
                }
            }
            continue;
        }
        let r = run_one(n, seed, bandwidth());
        println!(
            "ROW {} {} {} {:.3} {:.1} {:.1}",
            r.n, r.rounds, r.converged, r.sim_seconds, r.wall_ms, r.peak_rss_mb
        );
        rows.push(r);
    }

    print_table(&rows);
}
