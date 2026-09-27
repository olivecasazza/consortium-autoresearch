//! claw — parallel cluster command execution (replaces clush).
//!
//! Usage:
//!   claw -w node[1-5] echo hello        # run on nodes
//!   claw -w node[1-5] -b 'uname -r'     # gather identical output
//!   claw -w node[1-5] -l root uptime     # run as root
//!   claw --copy /etc/hosts -w node[1-5]  # copy file to nodes
//!   echo 'script' | claw -w node[1-5]    # pipe stdin to command

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process;
use std::time::{Duration, Instant};

use clap::Parser;

use consortium::node_set::NodeSet;
use consortium::node_utils::GroupResolverConfig;
use consortium::task::{Task, TaskError};
use consortium::worker::exec::ExecWorker;
use consortium::worker::ssh::SshOptions;
use consortium_cli::display;
use consortium_cli::output::{CliOutput, OutputArgs};

/// claw — execute commands in parallel across cluster nodes.
///
/// Runs shell commands on multiple nodes simultaneously with
/// configurable fanout, output gathering, and timeout control.
#[derive(Parser)]
#[command(name = "claw", version, about)]
struct Args {
    // ── Node selection ─────────────────────────────────────────────────
    /// Target nodes (bracket notation, e.g., node[1-5]).
    #[arg(short = 'w', long = "nodes")]
    nodes: Option<String>,

    /// Exclude nodes from the target set.
    #[arg(short = 'x', long = "exclude")]
    exclude: Vec<String>,

    /// Use all nodes from the default group.
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Use nodes from a named group.
    #[arg(short = 'g', long = "group")]
    group: Vec<String>,

    /// Exclude nodes from a named group.
    #[arg(short = 'X', long = "exclude-group")]
    exclude_group: Vec<String>,

    /// Pick N random nodes.
    #[arg(long = "pick")]
    pick: Option<usize>,

    /// Read nodes from a file (one per line).
    #[arg(long = "hostfile")]
    hostfile: Option<String>,

    // ── Execution ──────────────────────────────────────────────────────
    /// Maximum concurrent connections (fanout).
    #[arg(short = 'f', long = "fanout")]
    fanout: Option<usize>,

    /// Remote user for SSH connections.
    #[arg(short = 'l', long = "user")]
    user: Option<String>,

    /// SSH connect timeout in seconds.
    #[arg(short = 't', long = "connect-timeout")]
    connect_timeout: Option<u64>,

    /// Command timeout in seconds.
    #[arg(short = 'u', long = "command-timeout")]
    command_timeout: Option<u64>,

    /// Worker type: exec or ssh (default: ssh for remote, exec for localhost).
    #[arg(short = 'R', long = "worker")]
    worker: Option<String>,

    /// SSH options to pass through (e.g., "-o BatchMode=yes").
    #[arg(short = 'o', long = "options")]
    options: Option<String>,

    /// Remote shell command (default: ssh).
    #[arg(long = "remote", default_value = "ssh")]
    remote: String,

    /// Topology file for tree mode.
    #[arg(long = "topology")]
    topology: Option<String>,

    // ── Output ─────────────────────────────────────────────────────────
    /// Gather identical output together (dshbak mode).
    #[arg(short = 'b', long = "dshbak")]
    dshbak: bool,

    /// Line mode — no output gathering.
    #[arg(short = 'L', long = "line")]
    line: bool,

    /// Disable node labels on output.
    #[arg(short = 'N', long = "label")]
    no_label: bool,

    /// Quiet mode — suppress output, just return exit code.
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Return the maximum exit code from all nodes.
    #[arg(short = 'S', long = "maxrc")]
    maxrc: bool,

    #[command(flatten)]
    output: OutputArgs,

    // ── File transfer ──────────────────────────────────────────────────
    /// Copy a file to all target nodes.
    #[arg(long = "copy")]
    copy: Option<String>,

    /// Reverse copy — copy file from nodes to local.
    #[arg(long = "rcopy")]
    rcopy: Option<String>,

    /// Destination directory for copy operations.
    #[arg(long = "dest")]
    dest: Option<String>,

    // ── Positional ─────────────────────────────────────────────────────
    /// Command and arguments to execute.
    command: Vec<String>,
}

fn main() {
    let args = Args::parse();
    let exit_code = match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("claw: {e}");
            1
        }
    };
    process::exit(exit_code);
}

fn run(args: Args) -> anyhow::Result<i32> {
    let cli_out = CliOutput::from_args(&args.output);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // ── Resolve target nodes ───────────────────────────────────────────
    let target_nodes = resolve_nodes(&args)?;
    if target_nodes.is_empty() {
        anyhow::bail!("no target nodes specified (use -w, -a, -g, or --hostfile)");
    }

    if cli_out.is_verbose() {
        cli_out.info(format!(
            "claw: targeting {} node(s): {}",
            target_nodes.len(),
            target_nodes
        ));
    }

    // ── Build the command string ───────────────────────────────────────
    let command = if !args.command.is_empty() {
        args.command.join(" ")
    } else if !is_terminal::is_terminal(io::stdin()) {
        // Read command from stdin
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        buf.trim_end().to_string()
    } else {
        anyhow::bail!("no command specified");
    };

    if command.is_empty() {
        anyhow::bail!("empty command");
    }

    // ── Build SSH options if needed ────────────────────────────────────
    let _ssh_opts = build_ssh_options(&args);

    // ── Determine worker type ──────────────────────────────────────────
    let use_ssh = match args.worker.as_deref() {
        Some("exec") => false,
        Some("ssh") => true,
        Some(other) => anyhow::bail!("unknown worker type: {other} (use 'exec' or 'ssh')"),
        None => {
            // Auto-detect: if all nodes are localhost, use exec
            let all_local = target_nodes
                .iter()
                .all(|n| n == "localhost" || n == "127.0.0.1" || n == "::1");
            !all_local
        }
    };

    // ── Build the command with SSH wrapping if needed ───────────────────
    let node_list: Vec<String> = target_nodes.iter().collect();
    let fanout = args.fanout.unwrap_or(64);
    let timeout = args.command_timeout.map(Duration::from_secs);

    let effective_command = if use_ssh {
        // Build SSH command: ssh [options] [user@]%h command
        let mut ssh_cmd = args.remote.clone();
        if let Some(ref opts) = args.options {
            ssh_cmd.push(' ');
            ssh_cmd.push_str(opts);
        }
        if let Some(ref ct) = args.connect_timeout {
            ssh_cmd.push_str(&format!(" -o ConnectTimeout={ct}"));
        }
        if let Some(ref user) = args.user {
            ssh_cmd.push_str(&format!(" {user}@%h"));
        } else {
            ssh_cmd.push_str(" %h");
        }
        ssh_cmd.push(' ');
        ssh_cmd.push_str(&shell_escape(&command));
        ssh_cmd
    } else {
        command.clone()
    };

    // ── Create and run the task ────────────────────────────────────────
    let mut task = Task::new();
    let worker =
        ExecWorker::new(node_list.clone(), effective_command, fanout, timeout).with_stderr(true);

    // Set up progress tracking (only when stderr is a TTY and >1 node)
    let num_nodes = node_list.len();
    let show_progress = !args.quiet && is_terminal::is_terminal(io::stderr()) && num_nodes > 1;

    let (pb, state) = if show_progress {
        let (bar, state, handler) = display::create_progress(num_nodes);
        task.schedule(Box::new(worker), Some(Box::new(handler)), false);
        (Some(bar), Some(state))
    } else {
        task.schedule(Box::new(worker), None, false);
        (None, None)
    };

    let start = Instant::now();
    let task_timeout = args.command_timeout.map(|t| Duration::from_secs(t + 5));
    match task.run(task_timeout) {
        Ok(()) => {}
        Err(TaskError::Timeout) => {
            if !args.quiet {
                eprintln!("claw: command timed out");
            }
        }
        Err(e) => return Err(e.into()),
    }

    // Finalize progress display
    if let (Some(bar), Some(state)) = (&pb, &state) {
        display::finish_progress(bar, state, start.elapsed());
    }

    // ── Collect and display output ─────────────────────────────────────
    let max_rc = display_results(&task, &args, &mut out)?;

    if args.maxrc {
        Ok(max_rc.unwrap_or(0))
    } else if max_rc.is_some_and(|rc| rc != 0) {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Find groups.conf config files in standard locations.
fn find_groups_conf() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // User config: ~/.config/clustershell/groups.conf
    if let Some(home) = std::env::var_os("HOME") {
        let user_conf = PathBuf::from(home).join(".config/clustershell/groups.conf");
        if user_conf.exists() {
            paths.push(user_conf);
        }
    }

    // System config: /etc/clustershell/groups.conf
    let sys_conf = PathBuf::from("/etc/clustershell/groups.conf");
    if sys_conf.exists() {
        paths.push(sys_conf);
    }

    paths
}

/// Load a GroupResolverConfig from standard config file locations.
fn load_group_resolver() -> anyhow::Result<GroupResolverConfig> {
    let paths = find_groups_conf();
    if paths.is_empty() {
        anyhow::bail!(
            "no groups.conf found (checked ~/.config/clustershell/ and /etc/clustershell/)"
        );
    }
    Ok(GroupResolverConfig::new(paths, HashSet::new()))
}

/// Resolve target nodes from all sources.
fn resolve_nodes(args: &Args) -> anyhow::Result<NodeSet> {
    let mut ns = NodeSet::new();

    // -w / --nodes
    if let Some(ref pattern) = args.nodes {
        let parsed = NodeSet::parse(pattern)?;
        ns.update(&parsed);
    }

    // -g / --group
    if !args.group.is_empty() || args.all {
        let mut config = load_group_resolver()?;
        let resolver = config.resolver()?;

        if args.all {
            // -a: all nodes from default group source
            let all_nodes = resolver.all_nodes(None)?;
            for node in &all_nodes {
                let _ = ns.update_str(node);
            }
        }

        for group_name in &args.group {
            let nodes = resolver.group_nodes(group_name, None)?;
            for node in &nodes {
                let _ = ns.update_str(node);
            }
        }

        // -X / --exclude-group
        for group_name in &args.exclude_group {
            let nodes = resolver.group_nodes(group_name, None)?;
            for node in &nodes {
                let exclude_ns = NodeSet::parse(node)?;
                ns.difference_update(&exclude_ns);
            }
        }
    }

    // --hostfile
    if let Some(ref path) = args.hostfile {
        let content = std::fs::read_to_string(path)?;
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                let _ = ns.update_str(trimmed);
            }
        }
    }

    // Excludes (-x)
    for pat in &args.exclude {
        let other = NodeSet::parse(pat)?;
        ns.difference_update(&other);
    }

    // Pick random subset
    if let Some(n) = args.pick {
        let all: Vec<String> = ns.iter().collect();
        if n < all.len() {
            let mut new_ns = NodeSet::new();
            // Simple sequential pick (deterministic for now)
            for node in all.iter().take(n) {
                let _ = new_ns.update_str(node);
            }
            ns = new_ns;
        }
    }

    Ok(ns)
}

/// Build SSH options from CLI args.
fn build_ssh_options(args: &Args) -> SshOptions {
    let mut opts = SshOptions::default();
    if let Some(ref user) = args.user {
        opts.user = Some(user.clone());
    }
    if let Some(ct) = args.connect_timeout {
        opts.connect_timeout = Some(ct as u32);
    }
    if let Some(ref _extra) = args.options {
        // Parse "-o Key=Value" style options
        opts.ssh_path = args.remote.clone(); // String, not Option
                                             // Store raw options for passthrough
    }
    opts
}

/// Display results from a completed task.
fn display_results(task: &Task, args: &Args, out: &mut impl Write) -> anyhow::Result<Option<i32>> {
    if args.quiet {
        return Ok(task.max_retcode());
    }

    if args.dshbak {
        display_dshbak(task, out)?;
    } else {
        display_standard(task, args, out)?;
    }

    // Show return codes for non-zero exits
    let retcodes = task.iter_retcodes(None);
    for (rc, nodes) in &retcodes {
        if *rc != 0 {
            let mut ns = NodeSet::new();
            for node in nodes {
                let _ = ns.update_str(node);
            }
            eprintln!("claw: {ns}: exited with return code {rc}");
        }
    }

    // Show timeouts
    let timeout_count = task.num_timeout();
    if timeout_count > 0 {
        let timeout_nodes: Vec<&str> = task.iter_keys_timeout().collect();
        let mut ns = NodeSet::new();
        for node in &timeout_nodes {
            let _ = ns.update_str(node);
        }
        eprintln!("claw: {ns}: command timeout");
    }

    Ok(task.max_retcode())
}

/// Display output in dshbak (gathered) mode.
fn display_dshbak(task: &Task, out: &mut impl Write) -> anyhow::Result<()> {
    // Group nodes by their output
    let mut output_to_nodes: HashMap<Vec<u8>, NodeSet> = HashMap::new();

    // Iterate over all nodes that ran
    let retcodes = task.iter_retcodes(None);
    for (_rc, nodes) in &retcodes {
        for node in nodes {
            let buf = task.node_buffer(node).unwrap_or_default();
            let ns = output_to_nodes.entry(buf).or_default();
            let _ = ns.update_str(node);
        }
    }

    for (output, ns) in &output_to_nodes {
        display::print_gathered_header(&ns.to_string(), out)?;
        out.write_all(output)?;
        if !output.is_empty() && !output.ends_with(b"\n") {
            writeln!(out)?;
        }
    }

    Ok(())
}

/// Display output in standard (labeled line) mode.
fn display_standard(task: &Task, args: &Args, out: &mut impl Write) -> anyhow::Result<()> {
    let retcodes = task.iter_retcodes(None);

    for (_rc, nodes) in &retcodes {
        for node in nodes {
            let buf = task.node_buffer(node).unwrap_or_default();
            if buf.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if args.no_label {
                    writeln!(out, "{line}")?;
                } else {
                    display::print_line_with_label(node, line, out)?;
                }
            }
        }
    }

    Ok(())
}

/// Shell-escape a command string for SSH passthrough.
fn shell_escape(s: &str) -> String {
    // If it contains no special chars, return as-is
    if s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=' | ':' | ','))
    {
        return s.to_string();
    }
    // Single-quote escape
    format!("'{}'", s.replace('\'', "'\\''"))
}
