//! Builder health checking — probe builders for SSH connectivity and Nix store access.

use std::process::Command;
use std::time::Instant;

use crate::config::{Builder, FleetConfig};
use crate::error::{NixError, Result};

/// Health status for a single builder.
#[derive(Debug, Clone)]
pub struct HealthStatus {
    /// The builder configuration.
    pub builder: Builder,
    /// Whether the builder is healthy (SSH + nix store reachable).
    pub healthy: bool,
    /// Round-trip latency in milliseconds (if healthy).
    pub latency_ms: Option<u64>,
    /// Error message (if unhealthy).
    pub error: Option<String>,
}

/// Probe all builders in the fleet and return their health status.
pub fn check_builders(config: &FleetConfig) -> Vec<HealthStatus> {
    // TODO: parallelize with consortium's SshWorker + fanout
    config.builders.values().map(check_builder).collect()
}

/// Probe a single builder for health.
pub fn check_builder(builder: &Builder) -> HealthStatus {
    let start = Instant::now();

    // First check SSH connectivity
    let ssh_result = Command::new("ssh")
        .args([
            "-oStrictHostKeyChecking=no",
            "-oPasswordAuthentication=no",
            "-oConnectTimeout=5",
            "-oBatchMode=yes",
            "-l",
            &builder.user,
            &builder.host,
            "true",
        ])
        .output();

    match ssh_result {
        Err(e) => HealthStatus {
            builder: builder.clone(),
            healthy: false,
            latency_ms: None,
            error: Some(format!("SSH exec failed: {}", e)),
        },
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            HealthStatus {
                builder: builder.clone(),
                healthy: false,
                latency_ms: None,
                error: Some(format!("SSH connection failed: {}", stderr.trim())),
            }
        }
        Ok(_) => {
            let ssh_latency = start.elapsed().as_millis() as u64;

            // Now check nix store accessibility
            let store_uri = format!("{}://{}@{}", builder.protocol, builder.user, builder.host);
            let store_result = Command::new("nix")
                .args(["store", "ping", "--store", &store_uri])
                .output();

            match store_result {
                Err(e) => HealthStatus {
                    builder: builder.clone(),
                    healthy: false,
                    latency_ms: Some(ssh_latency),
                    error: Some(format!("nix store ping failed: {}", e)),
                },
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    HealthStatus {
                        builder: builder.clone(),
                        healthy: false,
                        latency_ms: Some(ssh_latency),
                        error: Some(format!("nix store unreachable: {}", stderr.trim())),
                    }
                }
                Ok(_) => HealthStatus {
                    builder: builder.clone(),
                    healthy: true,
                    latency_ms: Some(start.elapsed().as_millis() as u64),
                    error: None,
                },
            }
        }
    }
}

/// Get only healthy builders, sorted by speed factor (highest first).
pub fn healthy_builders(statuses: &[HealthStatus]) -> Vec<&HealthStatus> {
    let mut healthy: Vec<_> = statuses.iter().filter(|s| s.healthy).collect();
    healthy.sort_by(|a, b| b.builder.speed_factor.cmp(&a.builder.speed_factor));
    healthy
}

/// Pre-warm SSH connections to builders by establishing ControlMaster sockets.
pub fn warm_connections(builders: &[&HealthStatus], control_path: &str) -> Result<()> {
    for status in builders {
        let b = &status.builder;
        let output = Command::new("ssh")
            .args([
                "-oStrictHostKeyChecking=no",
                "-oPasswordAuthentication=no",
                "-oControlMaster=auto",
                &format!("-oControlPath={}", control_path),
                "-oControlPersist=10m",
                "-oBatchMode=yes",
                "-fN", // background, no command
                "-l",
                &b.user,
                &b.host,
            ])
            .output()
            .map_err(|e| NixError::SshFailed {
                host: b.host.clone(),
                message: format!("failed to warm connection: {}", e),
            })?;

        if !output.status.success() {
            // Non-fatal: just log and continue
            eprintln!(
                "warning: failed to warm SSH connection to {}: {}",
                b.host,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    Ok(())
}
