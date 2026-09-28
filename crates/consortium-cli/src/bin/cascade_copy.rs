//! cascade-copy — fan-out distribute a nix store path across a fleet
//! using the cascade primitive + real `nix copy` over SSH.
//!
//! Usage:
//!
//! ```text
//! cascade-copy <STORE_PATH> --inventory hosts.toml [opts]
//! ```
//!
//! Drives a real cascade against actual hosts:
//! - The seed host (from inventory) `nix copy`s the path to its
//!   tree children.
//! - As children receive the closure, they SSH-launch their own
//!   `nix copy` to THEIR children — log-N fan-out.
//! - Live tree visualization shows progress in real time.
//!
//! Differs from `cascade-viz live` (sim) in that EVERY edge is a
//! real subprocess call against actual hosts.

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use is_terminal::IsTerminal;

use consortium_cli::event_render::LiveTreeRenderer;
use consortium_cli::inventory::load_inventory;
use consortium_cli::output::{CliOutput, OutputArgs};
use consortium_nix::cascade::{Cascade, Log2FanOut, NetworkProfile};
use consortium_nix::cascade_executor::NixCopyExecutor;
use consortium_nix::cascade_strategies::{LevelTreeFanOut, MaxBottleneckSpanning, SteinerGreedy};

#[derive(Parser, Debug)]
#[command(
    name = "cascade-copy",
    about = "Fan-out distribute a nix store path across a fleet via cascade + real nix copy over SSH"
)]
struct Args {
    /// The nix store path to distribute (e.g. /nix/store/xxx-foo-1.0).
    /// Must already exist on the seed host.
    store_path: String,

    /// Path to the TOML inventory file with `seed` + `nodes` SSH addrs.
    #[arg(short = 'i', long = "inventory")]
    inventory: String,

    /// Cascade strategy: level-tree (default), log2-fanout,
    /// max-bottleneck, or steiner.
    #[arg(short = 's', long = "strategy", default_value = "level-tree")]
    strategy: String,

    /// Fanout for level-tree (children per node, default 2).
    #[arg(long = "fanout", default_value_t = 2)]
    fanout: u32,

    /// Per-edge `nix copy` subprocess timeout in seconds (default 300).
    #[arg(long = "timeout", default_value_t = 300)]
    timeout_secs: u64,

    /// Cascade hard-cap rounds — if not converged by this round count,
    /// give up. Default = 64.
    #[arg(long = "max-rounds", default_value_t = 64)]
    max_rounds: u32,

    /// Disable live re-rendering (useful when piping or for CI).
    #[arg(long = "no-watch")]
    no_watch: bool,

    /// Limit tree depth in the rendered output.
    #[arg(short = 'L', long = "max-depth")]
    max_depth: Option<usize>,

    #[command(flatten)]
    output: OutputArgs,
}

fn main() {
    let args = Args::parse();
    let exit_code = match run(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cascade-copy: {e:#}");
            1
        }
    };
    std::process::exit(exit_code);
}

fn run(args: Args) -> Result<i32> {
    let out = CliOutput::from_args(&args.output);

    // Load + validate inventory.
    let (nodes, seed, addrs) = load_inventory(&args.inventory)
        .with_context(|| format!("loading inventory from {}", args.inventory))?;
    if nodes.len() < 2 {
        anyhow::bail!(
            "inventory must have at least 1 seed + 1 target; got {} total nodes",
            nodes.len()
        );
    }

    out.info(format!(
        "loaded inventory: 1 seed ({}) + {} targets",
        nodes[0].addr,
        nodes.len() - 1
    ));
    out.info(format!(
        "store_path = {} | strategy = {} | fanout = {}",
        args.store_path, args.strategy, args.fanout
    ));

    // Seeded set: just NodeId(0) (the seed host).
    let mut seeded = std::collections::HashSet::new();
    seeded.insert(seed);

    // Empty network profile — no contention modeling for real
    // deploys (the OS + nix scheduler handle it). The strategy
    // ignores bandwidth/latency anyway for level-tree.
    let net = NetworkProfile::default();

    // The real-world executor.
    let executor = NixCopyExecutor::new(addrs, args.store_path.clone(), seed)
        .with_timeout(Duration::from_secs(args.timeout_secs));

    // Compose nh-style header lines for the live renderer.
    let mut header_lines: Vec<String> = Vec::new();
    header_lines.push(format!(
        "cascade-copy || Strategy: {} || Targets: {}",
        args.strategy,
        nodes.len() - 1
    ));
    header_lines.push(format!(
        "Seed: {} || Path: {}",
        nodes[0].addr, args.store_path
    ));
    if args.fanout != 2 {
        header_lines.push(format!("Fanout: {}", args.fanout));
    }

    let live_eligible = io::stdout().is_terminal() && !args.no_watch;
    let renderer = LiveTreeRenderer::new(out.color, args.max_depth).with_header_lines(header_lines);

    let level_tree = LevelTreeFanOut::new(args.fanout.max(1));
    let result = match args.strategy.as_str() {
        "level-tree" | "level" | "tree" => Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(net)
            .strategy(&level_tree)
            .executor(&executor)
            .max_rounds(args.max_rounds)
            .events(if live_eligible {
                &renderer as _
            } else {
                &consortium_nix::cascade_events::NullSink as _
            })
            .run(),
        "log2-fanout" | "log2" => Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(net)
            .strategy(&Log2FanOut)
            .executor(&executor)
            .max_rounds(args.max_rounds)
            .events(if live_eligible {
                &renderer as _
            } else {
                &consortium_nix::cascade_events::NullSink as _
            })
            .run(),
        "max-bottleneck" | "max-bottleneck-spanning" => Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(net)
            .strategy(&MaxBottleneckSpanning)
            .executor(&executor)
            .max_rounds(args.max_rounds)
            .events(if live_eligible {
                &renderer as _
            } else {
                &consortium_nix::cascade_events::NullSink as _
            })
            .run(),
        "steiner" | "steiner-greedy" => Cascade::new()
            .nodes(nodes)
            .seeded(seeded)
            .network(net)
            .strategy(&SteinerGreedy)
            .executor(&executor)
            .max_rounds(args.max_rounds)
            .events(if live_eligible {
                &renderer as _
            } else {
                &consortium_nix::cascade_events::NullSink as _
            })
            .run(),
        other => anyhow::bail!(
            "unknown strategy: {other} (use level-tree, log2-fanout, max-bottleneck, or steiner)"
        ),
    };

    let total: Duration = result.round_durations.iter().sum();
    out.info(format!(
        "done: {} converged / {} total in {} rounds ({:.1}s wall)",
        result.converged.len(),
        result.converged.len()
            + result
                .failed
                .iter()
                .flat_map(|e| e.affected_nodes())
                .count(),
        result.rounds,
        total.as_secs_f64()
    ));
    if let Some(err) = &result.failed {
        out.error(format!("failures: {err}"));
        Ok(1)
    } else {
        Ok(0)
    }
}
