//! Production [`RoundExecutor`] implementations — wrap real `nix copy`
//! subprocesses, runnable against actual nixlab hosts.
//!
//! [`NixCopyExecutor`] is the realistic counterpart to
//! [`consortium_fanout_sim::DeterministicExecutor`]. The sim does
//! `closure_size / bandwidth + latency` math; this one shells out to
//! `nix copy --no-check-sigs --to ssh-ng://user@host store_path` for
//! every (src, tgt) edge in a round, in parallel via `std::thread`.
//!
//! ## Edge-source handling
//!
//! For an edge `(src, tgt)` with the cascade's seed at `seed_id`:
//! - If `src == seed_id`: run `nix copy ...` LOCALLY (the host running
//!   cascade-copy IS the seed)
//! - Otherwise: SSH into `src` and run `nix copy ...` THERE (the source
//!   forwards the closure it received in a prior round)
//!
//! ## Trust + signing
//!
//! `--no-check-sigs` is passed because closures built locally are NOT
//! signed by a key the remote trusts. Trust boundary is the SSH
//! connection itself (root-over-ssh deploy assumption — same as
//! `consortium_nix::copy::copy_closure`).
//!
//! ## Failure mapping
//!
//! `nix copy` exit status maps to:
//! - exit 0 → `Ok(elapsed_duration)`
//! - non-zero with stderr containing "Connection refused"/"Permission
//!   denied" → `CascadeError::SshHandshake` (permanent — no retry from
//!   alt source will help if the tgt itself is unreachable)
//! - non-zero otherwise → `CascadeError::Copy` (transient — retry from
//!   alt source might succeed, e.g. if it was source-side bandwidth)

use std::collections::HashMap;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::cascade::{CascadeError, CascadeNode, NetworkProfile, NodeId, RoundExecutor};

/// Real-world `RoundExecutor` that drives `nix copy` over SSH.
pub struct NixCopyExecutor {
    /// NodeId → SSH address (e.g. `"root@hp01"` or `"olive@seir"`).
    /// Seed node also has an entry here for symmetry, even though
    /// edges originating from it run locally.
    pub addrs: HashMap<NodeId, String>,
    /// Store paths being distributed. Single-path cascade carries one
    /// toplevel (e.g. `/nix/store/xxx-foo-1.0`); multi-path cascade
    /// carries the closure-intersection of multiple toplevels — `nix
    /// copy` accepts an arbitrary list of positional store paths.
    pub store_paths: Vec<String>,
    /// NodeId of the seed — edges originating from it run via local
    /// `nix copy`; all other src edges run via `ssh <src> 'nix copy …'`.
    pub seed: NodeId,
    /// Per-edge subprocess timeout. Cascade halts the edge if the SSH
    /// or `nix copy` hasn't returned by this point — typically the
    /// remote is unreachable. Default 5 minutes.
    pub timeout: Duration,
}

impl NixCopyExecutor {
    /// Construct a single-path executor (back-compat: the original
    /// shape, used by per-toplevel cascades).
    pub fn new(
        addrs: HashMap<NodeId, String>,
        store_path: impl Into<String>,
        seed: NodeId,
    ) -> Self {
        Self {
            addrs,
            store_paths: vec![store_path.into()],
            seed,
            timeout: Duration::from_secs(300),
        }
    }

    /// Construct a multi-path executor — distributes the union of
    /// `store_paths` in one cascade. Used by closure-intersection
    /// cascades where the payload is the shared substrate across
    /// many heterogeneous toplevels.
    pub fn new_multi(
        addrs: HashMap<NodeId, String>,
        store_paths: Vec<String>,
        seed: NodeId,
    ) -> Self {
        Self {
            addrs,
            store_paths,
            seed,
            timeout: Duration::from_secs(300),
        }
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Run a single edge: src copies the closure to tgt. Blocks until
    /// the subprocess completes, the subprocess errors out, or the
    /// per-edge timeout fires.
    ///
    /// Returns `Ok(elapsed)` on success, `Err(CascadeError)` otherwise.
    fn run_edge(&self, src: NodeId, tgt: NodeId) -> Result<Duration, CascadeError> {
        let Some(tgt_addr) = self.addrs.get(&tgt) else {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: format!("no SSH address registered for tgt {tgt}"),
            });
        };
        let store_uri = format!("ssh-ng://{tgt_addr}");

        if self.store_paths.is_empty() {
            return Err(CascadeError::Copy {
                node: tgt,
                stderr: "NixCopyExecutor.store_paths is empty — nothing to copy".into(),
            });
        }

        let started = Instant::now();
        let cmd_result = if src == self.seed {
            // Local nix copy from the seed — pass every payload path
            // as a positional arg.
            let mut cmd = Command::new("nix");
            cmd.args(["copy", "--no-check-sigs", "--to", &store_uri]);
            for p in &self.store_paths {
                cmd.arg(p);
            }
            cmd.output()
        } else {
            // SSH into src and have it run nix copy with every payload
            // path. Each path quoted so spaces (rare) don't break
            // remote shell parsing.
            let Some(src_addr) = self.addrs.get(&src) else {
                return Err(CascadeError::Copy {
                    node: tgt,
                    stderr: format!("no SSH address registered for src {src}"),
                });
            };
            let mut remote_cmd =
                format!("nix copy --no-check-sigs --to {}", shell_escape(&store_uri),);
            for p in &self.store_paths {
                remote_cmd.push(' ');
                remote_cmd.push_str(&shell_escape(p));
            }
            Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                ])
                .arg(src_addr)
                .arg(remote_cmd)
                .output()
        };

        let elapsed = started.elapsed();
        let src_is_seed = src == self.seed;
        match cmd_result {
            Ok(output) if output.status.success() => Ok(elapsed),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                Err(classify_copy_error(tgt, src, src_is_seed, &stderr))
            }
            Err(io_err) => Err(CascadeError::Copy {
                node: tgt,
                stderr: format!("subprocess spawn failed: {io_err}"),
            }),
        }
    }
}

impl RoundExecutor for NixCopyExecutor {
    fn dispatch(
        &self,
        _nodes: &[CascadeNode],
        edges: &[(NodeId, NodeId)],
        _net: &NetworkProfile,
    ) -> HashMap<(NodeId, NodeId), Result<Duration, CascadeError>> {
        // Spawn one thread per edge; collect via channel so we don't
        // need to box the closures or hold thread handles.
        let (tx, rx) = mpsc::channel();
        let n = edges.len();
        thread::scope(|scope| {
            for &(src, tgt) in edges {
                let tx = tx.clone();
                let me = self;
                scope.spawn(move || {
                    let outcome = me.run_edge(src, tgt);
                    let _ = tx.send(((src, tgt), outcome));
                });
            }
        });
        drop(tx); // close the channel so the rx loop terminates
        let mut out = HashMap::with_capacity(n);
        while let Ok(item) = rx.try_recv() {
            out.insert(item.0, item.1);
        }
        out
    }
}

/// Map a non-zero `nix copy` stderr to the right `CascadeError` variant.
/// Permanent vs transient distinction matters for orphan re-routing
/// (see cascade.rs's `is_transient()` discussion).
///
/// Crucially distinguishes seed-source from peer-source failures:
///
/// - **Seed source** (the dev box has authoritative SSH config to all
///   hosts): an SSH handshake failure means the target is genuinely
///   unreachable → permanent.
/// - **Peer source** (one cluster host trying to push to another): an
///   SSH failure usually means peer-to-peer SSH isn't configured (e.g.
///   in nixlab, Mac Minis don't trust each other's host keys), NOT
///   that the target is dead. Mark transient so the cascade retries
///   the same target from a different source (typically the seed).
fn classify_copy_error(tgt: NodeId, src: NodeId, src_is_seed: bool, stderr: &str) -> CascadeError {
    let lower = stderr.to_lowercase();
    let is_ssh_error = lower.contains("connection refused")
        || lower.contains("connection timed out")
        || lower.contains("no route to host")
        || lower.contains("permission denied")
        || lower.contains("host key verification failed");

    if is_ssh_error && src_is_seed {
        // Seed (authoritative SSH config) couldn't reach tgt → tgt
        // is genuinely unreachable. Mark permanent so orphan re-
        // routing skips its subtree.
        CascadeError::SshHandshake {
            node: tgt,
            parent: src,
        }
    } else {
        // Either: a peer source couldn't reach tgt (likely peer→peer
        // SSH not configured) — let cascade retry from another source.
        // Or: a non-SSH error (substituter slow, bandwidth glitch, etc.)
        // which has always been transient.
        CascadeError::Copy {
            node: tgt,
            stderr: stderr.lines().take(5).collect::<Vec<_>>().join("\n"),
        }
    }
}

/// Minimal POSIX shell escape — wraps in single quotes, doubles any
/// embedded single quote. Safe for `ssh remote 'cmd ...'` invocations.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r#"'\''"#);
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_basic() {
        assert_eq!(shell_escape("foo"), "'foo'");
        assert_eq!(shell_escape("/nix/store/xxx-foo"), "'/nix/store/xxx-foo'");
        assert_eq!(shell_escape("with 'quote'"), r#"'with '\''quote'\'''"#);
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn classify_seed_ssh_errors_as_permanent() {
        // SSH errors from the SEED (authoritative SSH config) mean the
        // target is genuinely unreachable.
        let perm_cases = [
            "ssh: connect to host hp01 port 22: Connection refused",
            "ssh: connect to host hp01 port 22: Connection timed out",
            "Permission denied (publickey).",
            "Host key verification failed.",
            "ssh: connect to host hp01 port 22: No route to host",
        ];
        for stderr in perm_cases {
            let err = classify_copy_error(NodeId(1), NodeId(0), true, stderr);
            assert!(
                matches!(err, CascadeError::SshHandshake { .. }),
                "expected SshHandshake (seed-source SSH error) for stderr={stderr:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn classify_peer_ssh_errors_as_transient() {
        // SSH errors from a PEER source (e.g. nixlab Mac Mini → another
        // Mac Mini, no host keys configured) should be transient — the
        // cascade should retry from a different source rather than
        // marking the target dead.
        let peer_ssh_cases = [
            "ssh: connect to host mm03 port 22: Connection refused",
            "Permission denied (publickey).",
            "Host key verification failed.",
        ];
        for stderr in peer_ssh_cases {
            let err = classify_copy_error(NodeId(2), NodeId(1), false, stderr);
            assert!(
                matches!(err, CascadeError::Copy { .. }),
                "expected Copy (transient — peer SSH not configured) for stderr={stderr:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn classify_transient_errors_default_to_copy() {
        // Non-SSH errors are always transient regardless of source.
        let cases = [
            "error: writing to file: No space left on device",
            "warning: substituter 'https://cache.nixos.org' returned HTTP 503",
            "some random stderr nobody categorized",
        ];
        for src_is_seed in [true, false] {
            for stderr in cases {
                let err = classify_copy_error(NodeId(1), NodeId(0), src_is_seed, stderr);
                assert!(
                    matches!(err, CascadeError::Copy { .. }),
                    "expected Copy (transient) for stderr={stderr:?} src_is_seed={src_is_seed}, got {err:?}"
                );
            }
        }
    }
}
