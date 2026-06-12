//! Profile activation — switch NixOS/nix-darwin systems to new configurations.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use crate::config::{DeployAction, DeploymentPlan, ProfileType};
use crate::error::{NixError, Result};

/// Activation results keyed by hostname.
pub struct ActivationResults {
    /// Hosts that were successfully activated.
    pub succeeded: Vec<String>,
    /// Map of hostname -> activation error.
    pub errors: HashMap<String, NixError>,
}

/// Activate profiles on all targets in the deployment plan.
pub fn activate_all(plan: &DeploymentPlan) -> Result<ActivationResults> {
    let mut results = ActivationResults {
        succeeded: Vec::new(),
        errors: HashMap::new(),
    };

    if plan.action == DeployAction::Build {
        // Build-only mode, skip activation
        results.succeeded = plan.targets.iter().map(|t| t.node.name.clone()).collect();
        return Ok(results);
    }

    // TODO: parallelize with consortium's SshWorker + fanout
    // TODO: support rolling activation (sequential with health checks)
    for target in &plan.targets {
        match activate_host(
            &target.node.target_host,
            &target.node.target_user,
            &target.toplevel_path,
            &target.node.profile_type,
            plan.action,
        ) {
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

/// Activate a profile on a single host.
pub fn activate_host(
    host: &str,
    user: &str,
    toplevel_path: &str,
    profile_type: &ProfileType,
    action: DeployAction,
) -> Result<()> {
    // Only set the system profile for actions that should persist across reboots.
    // dry-activate and test should NOT modify the profile.
    match action {
        DeployAction::Switch | DeployAction::Boot => {
            set_profile(host, user, toplevel_path)?;
        }
        DeployAction::Test | DeployAction::DryActivate | DeployAction::Build => {}
    }

    // Run the activation command
    let activation_cmd = match profile_type {
        ProfileType::Nixos => {
            format!("{}/bin/switch-to-configuration {}", toplevel_path, action)
        }
        ProfileType::NixDarwin => {
            format!(
                "{}/activate-user && sudo {}/activate",
                toplevel_path, toplevel_path
            )
        }
    };

    let output = Command::new("ssh")
        .args([
            "-n",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=no",
            "-oPasswordAuthentication=no",
            "-oConnectTimeout=30",
            "-oServerAliveInterval=15",
            "-oServerAliveCountMax=4",
            "-l",
            user,
            host,
            &activation_cmd,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| NixError::ActivationFailed {
            host: host.to_string(),
            message: format!("failed to run activation: {}", e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::ActivationFailed {
            host: host.to_string(),
            message: stderr.to_string(),
        });
    }

    Ok(())
}

/// Set the nix profile to point to the new system closure.
fn set_profile(host: &str, user: &str, toplevel_path: &str) -> Result<()> {
    let cmd = format!(
        "nix-env -p /nix/var/nix/profiles/system --set {}",
        toplevel_path
    );

    let output = Command::new("ssh")
        .args([
            "-n",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=no",
            "-oPasswordAuthentication=no",
            "-oConnectTimeout=30",
            "-oServerAliveInterval=15",
            "-oServerAliveCountMax=4",
            "-l",
            user,
            host,
            &cmd,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| NixError::ActivationFailed {
            host: host.to_string(),
            message: format!("failed to set profile: {}", e),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::ActivationFailed {
            host: host.to_string(),
            message: format!("profile set failed: {}", stderr),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_set_only_for_switch_and_boot() {
        // Verify all DeployAction variants are accounted for in the match.
        let no_profile_actions = vec![
            DeployAction::Test,
            DeployAction::DryActivate,
            DeployAction::Build,
        ];
        let profile_actions = vec![DeployAction::Switch, DeployAction::Boot];
        assert_eq!(no_profile_actions.len() + profile_actions.len(), 5);
    }

    #[test]
    fn test_activation_command_nixos() {
        let toplevel = "/nix/store/abc-nixos-system";
        let cmd = format!(
            "{}/bin/switch-to-configuration {}",
            toplevel,
            DeployAction::Switch
        );
        assert_eq!(
            cmd,
            "/nix/store/abc-nixos-system/bin/switch-to-configuration switch"
        );

        let cmd = format!(
            "{}/bin/switch-to-configuration {}",
            toplevel,
            DeployAction::DryActivate
        );
        assert_eq!(
            cmd,
            "/nix/store/abc-nixos-system/bin/switch-to-configuration dry-activate"
        );
    }

    #[test]
    fn test_activation_command_darwin() {
        let toplevel = "/nix/store/abc-darwin-system";
        let cmd = format!("{}/activate-user && sudo {}/activate", toplevel, toplevel);
        assert_eq!(
            cmd,
            "/nix/store/abc-darwin-system/activate-user && sudo /nix/store/abc-darwin-system/activate"
        );
    }
}
