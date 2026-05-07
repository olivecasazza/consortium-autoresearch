//! Closure introspection — query the local nix store for what paths a
//! given toplevel depends on.
//!
//! Used by [`crate::deploy_with_cascade`] to compute the *shared
//! closure* across a heterogeneous fleet: the set of store paths that
//! every host in the deploy will need regardless of its individual
//! toplevel. Cascading those shared paths peer-to-peer (instead of one
//! representative toplevel) gets the fan-out win without sending any
//! host-specific bits to hosts that don't need them.
//!
//! ## What `nix path-info -r` returns
//!
//! `nix path-info -r /nix/store/AAA-system` walks AAA's reference graph
//! and prints every store path AAA transitively depends on, one per
//! line. This is the same closure `nix copy` would walk — so paths
//! present in *every* toplevel's `path-info -r` output are guaranteed
//! to be on every host post-cascade, and the per-host tail copy will
//! only transfer the per-host-unique paths.
//!
//! ## Performance
//!
//! `nix path-info -r` walks the local store metadata only — no network
//! IO, no hashing, just sqlite lookups. ~100–500ms per toplevel for
//! typical NixOS systems. We run them in parallel across toplevels.

use std::collections::HashSet;
use std::process::Command;
use std::sync::Mutex;
use std::thread;

use crate::error::{NixError, Result};

/// Return the full closure of `store_path` — every store path it
/// transitively depends on, including itself.
///
/// Runs `nix path-info -r STORE_PATH` against the local store. Errors
/// if the path isn't present locally or if nix returns non-zero.
pub fn closure_of(store_path: &str) -> Result<HashSet<String>> {
    let output = Command::new("nix")
        .args(["path-info", "-r", store_path])
        .output()
        .map_err(|e| NixError::General(format!("failed to spawn nix path-info: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::General(format!(
            "nix path-info -r {store_path} failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: HashSet<String> = stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok(paths)
}

/// Compute the set intersection of every toplevel's closure — paths
/// present in ALL of them. These are the "shared substrate" paths
/// every host in the deploy needs regardless of its individual
/// toplevel; cascading them gets the fan-out win without sending any
/// host-specific bits.
///
/// Runs `closure_of` for each toplevel in parallel. Returns an empty
/// vec for empty input. If closure introspection fails for any
/// toplevel the error propagates — caller decides whether to fall
/// back to the carrier-toplevel approach.
pub fn shared_paths(toplevels: &[String]) -> Result<Vec<String>> {
    if toplevels.is_empty() {
        return Ok(Vec::new());
    }
    if toplevels.len() == 1 {
        // Single toplevel: its closure IS the shared closure.
        let mut paths: Vec<String> = closure_of(&toplevels[0])?.into_iter().collect();
        paths.sort();
        return Ok(paths);
    }

    // Parallel introspection — each `nix path-info -r` is independent.
    let results: Mutex<Vec<Result<HashSet<String>>>> =
        Mutex::new(Vec::with_capacity(toplevels.len()));
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(toplevels.len());
        for tl in toplevels {
            let tl = tl.clone();
            handles.push(scope.spawn(move || closure_of(&tl)));
        }
        for h in handles {
            let r = h
                .join()
                .map_err(|_| NixError::General("closure_of thread panicked".into()))
                .and_then(|inner| inner);
            results.lock().unwrap().push(r);
        }
    });

    let mut closures: Vec<HashSet<String>> = Vec::with_capacity(toplevels.len());
    for r in results.into_inner().unwrap() {
        closures.push(r?);
    }

    // Intersect — start with the smallest closure to keep the running
    // intersection small.
    closures.sort_by_key(|c| c.len());
    let mut iter = closures.into_iter();
    let mut acc = iter.next().unwrap();
    for c in iter {
        acc.retain(|p| c.contains(p));
    }

    let mut paths: Vec<String> = acc.into_iter().collect();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        let r = shared_paths(&[]).unwrap();
        assert!(r.is_empty());
    }
}
