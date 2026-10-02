//! molt — aggregate cluster output (replaces clubak).
//!
//! Reads "node:output" lines from stdin or files and aggregates them.
//!
//! Usage:
//!   some_command | molt -b        # gather nodes with identical output
//!   some_command | molt -L        # line mode, ordered by node
//!   molt -b < output.txt         # from file

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::process;

use clap::Parser;

use consortium::msg_tree::{MsgTree, MsgTreeMode};
use consortium::node_set::NodeSet;
use consortium_cli::display;
use consortium_cli::output::{CliOutput, OutputArgs};

/// molt — aggregate and display cluster command output.
///
/// Reads "key: output" lines from stdin (like dsh/pdsh output) and
/// groups nodes that produced identical output together.
#[derive(Parser)]
#[command(name = "molt", version, about)]
struct Args {
    /// Gather nodes with identical output (dshbak mode).
    #[arg(short = 'b', long = "dshbak")]
    dshbak: bool,

    /// Line mode — disable header, order by node name.
    #[arg(short = 'L', long = "line")]
    line: bool,

    /// Key separator (default: ':').
    #[arg(short = 'S', long = "separator", default_value = ":")]
    separator: String,

    /// Message tree trace mode.
    #[arg(short = 'T', long = "tree")]
    tree: bool,

    /// No group source prefix in regroup output.
    #[arg(short = 'G', long = "groupbase")]
    groupbase: bool,

    /// Input files (reads stdin if none given).
    files: Vec<String>,

    #[command(flatten)]
    output: OutputArgs,
}

fn main() {
    let args = Args::parse();
    let _out = CliOutput::from_args(&args.output);
    if let Err(e) = run(args) {
        eprintln!("molt: {e}");
        process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Read all input lines
    let lines = read_input(&args)?;

    if args.dshbak {
        run_dshbak(&lines, &args.separator, &mut out)?;
    } else if args.line {
        run_line_mode(&lines, &args.separator, &mut out)?;
    } else {
        // Default: pass through with node labels
        run_passthrough(&lines, &mut out)?;
    }

    Ok(())
}

/// Read input from files or stdin.
fn read_input(args: &Args) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::new();

    if args.files.is_empty() {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            lines.push(line?);
        }
    } else {
        for path in &args.files {
            let file = std::fs::File::open(path)?;
            let reader = io::BufReader::new(file);
            for line in reader.lines() {
                lines.push(line?);
            }
        }
    }

    Ok(lines)
}

/// Parse a line into (key, message) using the separator.
fn parse_line<'a>(line: &'a str, separator: &str) -> Option<(&'a str, &'a str)> {
    let idx = line.find(separator)?;
    let key = line[..idx].trim();
    let msg = line[idx + separator.len()..].trim_start();
    if key.is_empty() {
        return None;
    }
    Some((key, msg))
}

/// dshbak mode: group nodes with identical output.
fn run_dshbak(lines: &[String], separator: &str, out: &mut impl Write) -> anyhow::Result<()> {
    // Build a MsgTree keyed by node name
    let mut tree = MsgTree::new(MsgTreeMode::Shift);

    for line in lines {
        if let Some((key, msg)) = parse_line(line, separator) {
            tree.add(key.to_string(), msg.as_bytes().to_vec());
        }
    }

    // Walk the tree — groups nodes with identical messages
    // walk yields (message: Vec<u8>, keys: Vec<String>)
    let mut groups: BTreeMap<Vec<u8>, NodeSet> = BTreeMap::new();

    for (message, keys) in tree.walk(None) {
        let ns = groups.entry(message).or_default();
        for key in keys {
            let _ = ns.update_str(&key);
        }
    }

    for (message, ns) in &groups {
        display::print_gathered_header(&ns.to_string(), out)?;
        out.write_all(message)?;
        if !message.ends_with(b"\n") {
            writeln!(out)?;
        }
    }

    Ok(())
}

/// Line mode: output ordered by node name.
fn run_line_mode(lines: &[String], separator: &str, out: &mut impl Write) -> anyhow::Result<()> {
    // Collect per-node output, then sort by node name
    let mut node_output: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for line in lines {
        if let Some((key, msg)) = parse_line(line, separator) {
            node_output
                .entry(key.to_string())
                .or_default()
                .push(msg.to_string());
        }
    }

    for (node, msgs) in &node_output {
        for msg in msgs {
            display::print_line_with_label(node, msg, out)?;
        }
    }

    Ok(())
}

/// Passthrough: just print lines as-is.
fn run_passthrough(lines: &[String], out: &mut impl Write) -> anyhow::Result<()> {
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
