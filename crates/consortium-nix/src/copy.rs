//! Closure copying — transfer built closures to deployment targets.

use std::collections::HashMap;
use std::process::Command;

use crate::config::DeploymentPlan;
use crate::error::{NixError, Result};

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
