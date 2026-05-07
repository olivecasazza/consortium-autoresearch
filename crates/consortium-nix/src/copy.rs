//! Closure copying — transfer built closures to deployment targets.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::thread;

use crate::cascade_integration::CascadeCopyTarget;
use crate::config::DeploymentPlan;
use crate::error::{NixError, Result};

/// Run a `nix copy` per target in parallel, returning per-host failures.
/// Used by the cascade's per-host diff phase: after the shared substrate
/// has been distributed via the fan-out tree, each host that needs a
/// different actual toplevel runs its own `nix copy` from seed. nix's
/// content-addressed store ensures only the missing tail is transferred,
/// so these copies are tiny.
pub fn parallel_diff_copy(targets: &[&CascadeCopyTarget]) -> Vec<(String, String)> {
    let failures: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    thread::scope(|scope| {
        for t in targets {
            let target = *t;
            let failures_ref = &failures;
            scope.spawn(move || {
                let store_uri = format!("ssh-ng://{}", target.ssh_addr);
                if let Err(e) = copy_closure(&target.toplevel_path, &store_uri) {
                    failures_ref
                        .lock()
                        .unwrap()
                        .push((target.host_name.clone(), format!("{e}")));
                }
            });
        }
    });
    failures.into_inner().unwrap()
}

/// Copy results keyed by hostname.
pub struct CopyResults {
    /// Hosts that were successfully copied to.
    pub succeeded: Vec<String>,
    /// Map of hostname -> copy error.
    pub errors: HashMap<String, NixError>,
}

/// Copy closures to all targets in the deployment plan.
pub fn copy_closures(plan: &DeploymentPlan) -> Result<CopyResults> {
    let mut results = CopyResults {
        succeeded: Vec::new(),
        errors: HashMap::new(),
    };

    // TODO: parallelize with consortium's Task/Worker fanout
    for target in &plan.targets {
        if !target.needs_copy {
            results.succeeded.push(target.node.name.clone());
            continue;
        }

        let store_uri = format!(
            "ssh-ng://{}@{}",
            target.node.target_user, target.node.target_host
        );

        match copy_closure(&target.toplevel_path, &store_uri) {
            Ok(()) => {
                results.succeeded.push(target.node.name.clone());
            }
            Err(e) => {
                results.errors.insert(target.node.name.clone(), e);
            }
        }
    }

    Ok(results)
}

/// Copy a single closure to a remote store.
///
/// Uses `--no-check-sigs` because locally-built closures aren't signed
/// by a key the remote trusts. We're deploying as root over SSH, so
/// the trust boundary is the SSH connection itself.
pub fn copy_closure(store_path: &str, store_uri: &str) -> Result<()> {
    let output = Command::new("nix")
        .args(["copy", "--no-check-sigs", "--to", store_uri, store_path])
        .output()
        .map_err(|e| NixError::CopyFailed {
            host: store_uri.to_string(),
            message: format!("failed to run nix copy: {}", e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::CopyFailed {
            host: store_uri.to_string(),
            message: stderr.to_string(),
        });
    }

    Ok(())
}
