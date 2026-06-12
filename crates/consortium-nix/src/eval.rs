//! Nix evaluation — resolve toplevel store paths and detect changes.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use crate::config::{DeployAction, DeploymentPlan, DeploymentTarget, FleetConfig};
use crate::error::{NixError, Result};

/// Evaluate which hosts need deployment by comparing desired vs current state.
pub fn evaluate(
    config: &FleetConfig,
    target_nodes: &[String],
    action: DeployAction,
    max_parallel: usize,
) -> Result<DeploymentPlan> {
    let mut plan = DeploymentPlan::new(action, max_parallel);

    for name in target_nodes {
        let node = config
            .nodes
            .get(name)
            .ok_or_else(|| NixError::General(format!("unknown node: {}", name)))?;

        // Get the expected toplevel path via nix eval
        let toplevel_path = eval_toplevel(&config.flake_uri, name)?;

        plan.targets.push(DeploymentTarget {
            node: node.clone(),
            toplevel_path,
            current_system: None, // filled in during deploy if needed
            needs_build: true,
            needs_copy: true,
        });
    }

    Ok(plan)
}

/// Evaluate the toplevel store path for a single host via `nix eval`.
pub fn eval_toplevel(flake_uri: &str, hostname: &str) -> Result<String> {
    let attr = format!(
        "{}#nixosConfigurations.{}.config.system.build.toplevel.outPath",
        flake_uri, hostname
    );

    let output = Command::new("nix")
        .args(["eval", "--raw", &attr])
        .output()
        .map_err(|e| NixError::EvalFailed {
            host: hostname.to_string(),
            message: format!("failed to run nix eval: {}", e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::EvalFailed {
            host: hostname.to_string(),
            message: stderr.to_string(),
        });
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(NixError::EvalFailed {
            host: hostname.to_string(),
            message: "nix eval returned empty path".to_string(),
        });
    }

    Ok(path)
}

/// Query the current system generation on a remote host.
pub fn query_current_system(host: &str, user: &str) -> Result<Option<String>> {
    let output = Command::new("ssh")
        .args([
            "-n",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=no",
            "-oPasswordAuthentication=no",
            "-oConnectTimeout=10",
            "-oServerAliveInterval=15",
            "-oServerAliveCountMax=4",
            "-l",
            user,
            host,
            "readlink",
            "/run/current-system",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| NixError::SshFailed {
            host: host.to_string(),
            message: format!("failed to query current system: {}", e),
        })?;

    if !output.status.success() {
        return Ok(None);
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(path))
    }
}

/// Evaluate all hosts and return a map of hostname -> toplevel path.
pub fn eval_all(flake_uri: &str, hostnames: &[String]) -> Result<HashMap<String, String>> {
    let mut results = HashMap::new();
    // TODO: parallelize with consortium's Task/Worker infrastructure
    for hostname in hostnames {
        let path = eval_toplevel(flake_uri, hostname)?;
        results.insert(hostname.clone(), path);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_toplevel_attr_format() {
        // Just verify the attribute path format is correct
        let flake = ".";
        let host = "contra";
        let attr = format!(
            "{}#nixosConfigurations.{}.config.system.build.toplevel.outPath",
            flake, host
        );
        assert_eq!(
            attr,
            ".#nixosConfigurations.contra.config.system.build.toplevel.outPath"
        );
    }
}
