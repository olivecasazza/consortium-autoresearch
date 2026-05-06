//! cast — NixOS deployment orchestration CLI.
//!
//! Usage:
//!   cast deploy --on hp[01-03] switch     # build, copy, activate
//!   cast build --on contra                # build only
//!   cast eval --on mm[01-05]              # show what would be built
//!   cast health                           # probe all builders
//!   cast status --on hp[01-03]            # show current system versions

use std::io;
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use is_terminal::IsTerminal;

use consortium::node_set::NodeSet;
use consortium_cli::event_render::LiveTreeRenderer;
use consortium_cli::output::{CliOutput, OutputArgs};
use consortium_nix::config::{DeployAction, FleetConfig};
use consortium_nix::health;

/// cast — NixOS deployment orchestration powered by consortium.
#[derive(Parser)]
#[command(name = "cast", version, about)]
struct Args {
    /// Path to fleet configuration JSON file.
    #[arg(short, long, default_value = "fleet.json")]
    config: PathBuf,

    /// Override the flake URI from fleet config (e.g. /home/user/nixlab or github:user/repo).
    #[arg(long)]
    flake: Option<String>,

    #[command(flatten)]
    output: OutputArgs,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Evaluate which hosts need deployment.
    Eval {
        /// Target nodes (bracket notation, e.g., hp[01-03]).
        #[arg(short = 'w', long = "on")]
        on: Option<String>,

        /// Target nodes by tag.
        #[arg(short = 'g', long = "tag")]
        tag: Vec<String>,
    },

    /// Build system closures.
    Build {
        /// Target nodes (bracket notation).
        #[arg(short = 'w', long = "on")]
        on: Option<String>,

        /// Target nodes by tag.
        #[arg(short = 'g', long = "tag")]
        tag: Vec<String>,

        /// Use distributed builders.
        #[arg(long)]
        builders: bool,

        /// Maximum parallel builds.
        #[arg(short = 'f', long = "fanout", default_value = "4")]
        fanout: usize,
    },

    /// Deploy to targets (build + copy + activate).
    Deploy {
        /// Target nodes (bracket notation).
        #[arg(short = 'w', long = "on")]
        on: Option<String>,

        /// Target nodes by tag.
        #[arg(short = 'g', long = "tag")]
        tag: Vec<String>,

        /// Deployment action.
        #[arg(default_value = "switch")]
        action: String,

        /// Use distributed builders.
        #[arg(long)]
        builders: bool,

        /// Maximum parallel operations.
        #[arg(short = 'f', long = "fanout", default_value = "4")]
        fanout: usize,

        /// Use the cascade primitive for the copy stage — peer-to-peer
        /// fan-out instead of per-host serial. Each host that has the
        /// closure joins the source pool for the next round, dropping
        /// copy time from O(N) to O(log N) for hosts sharing a toplevel.
        #[arg(long = "cascade")]
        cascade: bool,

        /// Cascade tree fanout (children per node). Only meaningful with
        /// --cascade. 2 = binary tree (default); higher values trade
        /// per-source bandwidth contention for fewer rounds.
        #[arg(long = "cascade-fanout", default_value = "2")]
        cascade_fanout: u32,
    },

    /// Probe builder health.
    Health,

    /// Show current system versions on targets.
    Status {
        /// Target nodes (bracket notation).
        #[arg(short = 'w', long = "on")]
        on: Option<String>,

        /// Target nodes by tag.
        #[arg(short = 'g', long = "tag")]
        tag: Vec<String>,
    },
}

fn main() {
    let args = Args::parse();
    let out = CliOutput::from_args(&args.output);

    let mut config = match FleetConfig::from_file(&args.config) {
        Ok(c) => c,
        Err(e) => {
            out.error(format!("{}", e));
            eprintln!(
                "hint: build fleet config first with: nix build .#fleet-config && cp result fleet.json"
            );
            process::exit(1);
        }
    };

    // Override flake URI if --flake was provided
    if let Some(ref flake) = args.flake {
        config.flake_uri = flake.clone();
    }

    let result = match args.command {
        Commands::Eval { on, tag } => cmd_eval(&config, on.as_deref(), &tag),
        Commands::Build {
            on,
            tag,
            builders,
            fanout,
        } => cmd_deploy(
            &config,
            on.as_deref(),
            &tag,
            "build",
            builders,
            fanout,
            false,
            2,
        ),
        Commands::Deploy {
            on,
            tag,
            action,
            builders,
            fanout,
            cascade,
            cascade_fanout,
        } => cmd_deploy(
            &config,
            on.as_deref(),
            &tag,
            &action,
            builders,
            fanout,
            cascade,
            cascade_fanout,
        ),
        Commands::Health => cmd_health(&config),
        Commands::Status { on, tag } => cmd_status(&config, on.as_deref(), &tag),
    };

    if let Err(e) = result {
        eprintln!("error: {}", e);
        process::exit(1);
    }
}

/// Resolve target nodes from --on and --tag flags, falling back to all nodes.
fn resolve_targets(
    config: &FleetConfig,
    on: Option<&str>,
    tags: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut targets = Vec::new();

    if let Some(pattern) = on {
        let ns = NodeSet::parse(pattern)
            .map_err(|e| anyhow::anyhow!("invalid node pattern '{}': {}", pattern, e))?;
        targets.extend(ns.iter());
    }

    if !tags.is_empty() {
        let tag_nodes = config.nodes_by_tags(tags);
        for node in tag_nodes {
            if !targets.contains(&node.name) {
                targets.push(node.name.clone());
            }
        }
    }

    // Default to all nodes if nothing specified
    if targets.is_empty() {
        targets = config.node_names();
    }

    // Validate all targets exist in config
    for name in &targets {
        if !config.nodes.contains_key(name) {
            anyhow::bail!(
                "unknown node '{}' (available: {})",
                name,
                config.node_names().join(", ")
            );
        }
    }

    Ok(targets)
}

fn cmd_eval(config: &FleetConfig, on: Option<&str>, tags: &[String]) -> anyhow::Result<()> {
    let targets = resolve_targets(config, on, tags)?;

    println!("Evaluating {} host(s):", targets.len());
    for name in &targets {
        let node = &config.nodes[name];
        println!(
            "  {} → {} ({}@{}, tags: [{}])",
            name,
            node.system,
            node.target_user,
            node.target_host,
            node.tags.join(", ")
        );
        if let Some(ref drv) = node.drv_path {
            println!("    drv: {}", drv);
        }
    }

    Ok(())
}

fn cmd_deploy(
    config: &FleetConfig,
    on: Option<&str>,
    tags: &[String],
    action_str: &str,
    use_builders: bool,
    fanout: usize,
    cascade: bool,
    cascade_fanout: u32,
) -> anyhow::Result<()> {
    let targets = resolve_targets(config, on, tags)?;
    let action: DeployAction = action_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid action '{}' (try: switch, boot, test, dry-activate, build)",
            action_str
        )
    })?;

    println!(
        "Deploying {} host(s) with action '{}'{}:",
        targets.len(),
        action,
        if cascade {
            format!(
                " [cascade copy fanout={} — peer-to-peer fan-out]",
                cascade_fanout
            )
        } else {
            String::new()
        }
    );
    for name in &targets {
        println!("  {}", name);
    }

    let report = if cascade && action != DeployAction::Build {
        // Determine seed addr — the host running cast IS the seed
        // (closure was built locally, or fetched here). The display
        // name is just for the live UI; NixCopyExecutor uses local
        // `nix copy` regardless.
        let seed_addr = std::env::var("USER")
            .map(|u| format!("{}@localhost", u))
            .unwrap_or_else(|_| "localhost".into());

        // Wire up the nh-style live tree renderer for the cascade
        // copy phase. Only when stdout is a TTY — otherwise the
        // cursor-positioning escape codes garble pipes / CI logs.
        // Color = true is safe since the TTY check already implies
        // an interactive terminal that handles ANSI; users who need
        // NO_COLOR can set the env var (console crate respects it).
        let renderer = io::stdout().is_terminal().then(|| {
            LiveTreeRenderer::new(true, None).with_header_lines(vec![
                format!(
                    "cast deploy --cascade || {} hosts || fanout: {}",
                    targets.len(),
                    cascade_fanout
                ),
                format!("Seed: {} || Action: {}", seed_addr, action),
            ])
        });
        let event_sink = renderer
            .as_ref()
            .map(|r| r as &dyn consortium_nix::cascade_events::EventSink);

        consortium_nix::deploy_with_cascade(
            config,
            &targets,
            action,
            fanout,
            use_builders,
            cascade_fanout,
            &seed_addr,
            event_sink,
        )?
    } else {
        consortium_nix::deploy(config, &targets, action, fanout, use_builders)?
    };

    println!();
    if report.is_success() {
        println!("Deployment successful!");
        println!("  Built: {}", report.built.len());
        if action != DeployAction::Build {
            println!("  Copied: {}", report.copied.len());
            println!("  Activated: {}", report.activated.len());
        }
    } else {
        println!(
            "Deployment completed with {} failure(s):",
            report.failure_count()
        );
        for (name, err) in &report.build_failures {
            eprintln!("  build failed on {}: {}", name, err);
        }
        for (name, err) in &report.copy_failures {
            eprintln!("  copy failed to {}: {}", name, err);
        }
        for (name, err) in &report.activation_failures {
            eprintln!("  activation failed on {}: {}", name, err);
        }
        process::exit(1);
    }

    Ok(())
}

fn cmd_health(config: &FleetConfig) -> anyhow::Result<()> {
    if config.builders.is_empty() {
        println!("No builders configured.");
        return Ok(());
    }

    println!("Probing {} builder(s):", config.builders.len());

    let statuses = health::check_builders(config);
    let healthy_count = statuses.iter().filter(|s| s.healthy).count();

    for status in &statuses {
        let icon = if status.healthy { "ok" } else { "FAIL" };
        let latency = status
            .latency_ms
            .map(|ms| format!(" ({}ms)", ms))
            .unwrap_or_default();
        let error = status
            .error
            .as_ref()
            .map(|e| format!(" - {}", e))
            .unwrap_or_default();

        println!(
            "  [{}] {}@{}{}{} (jobs:{}, speed:{}x)",
            icon,
            status.builder.user,
            status.builder.host,
            latency,
            error,
            status.builder.max_jobs,
            status.builder.speed_factor,
        );
    }

    println!("\n{}/{} builders healthy", healthy_count, statuses.len());

    Ok(())
}

fn cmd_status(config: &FleetConfig, on: Option<&str>, tags: &[String]) -> anyhow::Result<()> {
    let targets = resolve_targets(config, on, tags)?;

    println!("Querying {} host(s):", targets.len());

    for name in &targets {
        let node = &config.nodes[name];
        match consortium_nix::eval::query_current_system(&node.target_host, &node.target_user) {
            Ok(Some(path)) => println!("  {} → {}", name, path),
            Ok(None) => println!("  {} → (unknown)", name),
            Err(e) => println!("  {} → error: {}", name, e),
        }
    }

    Ok(())
}
