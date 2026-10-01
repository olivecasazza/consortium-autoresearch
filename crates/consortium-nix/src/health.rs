//! Builder health checking — probe builders for SSH connectivity and Nix store access.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use consortium::worker::ssh::SshWorker;
use consortium::worker::Worker;

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

/// Probe all builders in the fleet and return their health status (parallelized).
///
/// Uses SshWorker to fan out SSH connectivity tests across all builders concurrently,
/// then performs sequential nix store pings on healthy builders.
pub fn check_builders(config: &FleetConfig) -> Vec<HealthStatus> {
    if config.builders.is_empty() {
        return Vec::new();
    }

    let builders: Vec<_> = config.builders.values().cloned().collect();
    let node_names: Vec<String> = builders.iter().map(|b| b.host.clone()).collect();

    // Phase 1: Parallelize SSH connectivity tests via SshWorker
    let ssh_health = parallel_ssh_check(&builders, &node_names);

    // Phase 2: Sequential nix store pings on healthy builders (depends on SSH results)
    builders
        .into_iter()
        .map(|builder| {
            if let Some(mut status) = ssh_health.get(&builder.host).cloned() {
                if status.healthy {
                    // SSH succeeded; now check nix store accessibility
                    complete_health_check(&builder, &mut status);
                }
                status
            } else {
                // Should not happen, but fallback
                HealthStatus {
                    builder: builder.clone(),
                    healthy: false,
                    latency_ms: None,
                    error: Some("parallel SSH check failed to report".to_string()),
                }
            }
        })
        .collect()
}

/// Perform parallelized SSH connectivity checks using SshWorker.
///
/// Returns a HashMap of host → partial HealthStatus (with only SSH test results).
fn parallel_ssh_check(
    builders: &[Builder],
    node_names: &[String],
) -> HashMap<String, HealthStatus> {
    let mut worker = SshWorker::with_defaults(
        node_names.to_vec(),
        "true".to_string(),
        // fanout: limited to 10 concurrent SSH connections for builders
        10,
        Some(Duration::from_secs(5)),
    );

    // Enable stderr capture to collect error messages
    worker = worker.with_stderr(true);

    // Start the worker (this spawns SSH processes)
    if let Err(e) = worker.start() {
        // If worker startup fails, fall back to sequential checks
        eprintln!("warning: parallel SSH health check startup failed: {}", e);
        return builders
            .iter()
            .map(|b| (b.host.clone(), check_ssh_only(&b)))
            .collect();
    }

    // Wait for all SSH commands to complete
    let start = Instant::now();
    wait_for_worker(&mut worker, Duration::from_secs(60));

    let ssh_latency = start.elapsed().as_millis() as u64;
    let retcodes = worker.retcodes().clone();

    // Collect results into a HashMap
    let mut result = HashMap::new();
    for builder in builders.iter() {
        let status = if let Some(&rc) = retcodes.get(&builder.host) {
            if rc == 0 {
                HealthStatus {
                    builder: builder.clone(),
                    healthy: true,
                    latency_ms: Some(ssh_latency),
                    error: None,
                }
            } else {
                HealthStatus {
                    builder: builder.clone(),
                    healthy: false,
                    latency_ms: None,
                    error: Some(format!("SSH connection failed (exit code: {})", rc)),
                }
            }
        } else {
            HealthStatus {
                builder: builder.clone(),
                healthy: false,
                latency_ms: None,
                error: Some("SSH check did not complete".to_string()),
            }
        };
        result.insert(builder.host.clone(), status);
    }

    result
}

/// Poll the worker until it completes or times out.
fn wait_for_worker(worker: &mut dyn Worker, timeout: Duration) {
    let start = Instant::now();
    loop {
        if worker.is_done() {
            return;
        }

        if start.elapsed() > timeout {
            worker.abort(true);
            return;
        }

        // Brief sleep to avoid busy-polling
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Check SSH connectivity only (used as fallback or internal helper).
fn check_ssh_only(builder: &Builder) -> HealthStatus {
    let start = Instant::now();

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
            HealthStatus {
                builder: builder.clone(),
                healthy: true,
                latency_ms: Some(ssh_latency),
                error: None,
            }
        }
    }
}

/// Complete health check by probing nix store (called after SSH is confirmed healthy).
fn complete_health_check(builder: &Builder, status: &mut HealthStatus) {
    // Now check nix store accessibility
    let store_uri = format!("{}://{}@{}", builder.protocol, builder.user, builder.host);
    let store_result = Command::new("nix")
        .args(["store", "ping", "--store", &store_uri])
        .output();

    match store_result {
        Err(e) => {
            status.healthy = false;
            status.error = Some(format!("nix store ping failed: {}", e));
        }
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            status.healthy = false;
            status.error = Some(format!("nix store unreachable: {}", stderr.trim()));
        }
        Ok(_) => {
            // Both SSH and nix store are healthy
            status.healthy = true;
            status.error = None;
        }
    }
}

/// Probe a single builder for health (fallback function for backward compatibility).
///
/// This is primarily used by tests or when parallelization isn't needed.
pub fn check_builder(builder: &Builder) -> HealthStatus {
    let mut status = check_ssh_only(builder);
    if status.healthy {
        complete_health_check(builder, &mut status);
    }
    status
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
