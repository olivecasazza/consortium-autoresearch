//! Nix build orchestration — build closures locally or with distributed builders.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::process::Command;
use std::sync::{Arc, Mutex};

use consortium::dag::types::{FnTask, TaskOutcome};
use consortium::dag::DagBuilder;

use crate::config::DeploymentPlan;
use crate::error::{NixError, Result};
use crate::health::HealthStatus;

/// Build results keyed by hostname.
pub struct BuildResults {
    /// Map of hostname -> built store path.
    pub paths: HashMap<String, String>,
    /// Map of hostname -> build error.
    pub errors: HashMap<String, NixError>,
}

/// Build all closures in a deployment plan.
///
/// If healthy builders are provided, generates a temporary machines file
/// and uses Nix's native distributed build mechanism.
///
/// Parallelizes build operations via consortium's DAG executor, respecting
/// the plan's max_parallel concurrency limit.
pub fn build_closures(
    plan: &DeploymentPlan,
    flake_uri: &str,
    healthy_builders: Option<&[HealthStatus]>,
) -> Result<BuildResults> {
    // Generate temporary machines file if we have healthy builders
    let machines_file = healthy_builders
        .map(|builders| generate_machines_file_from_healthy(builders))
        .transpose()?;

    // Shared results collected by DAG tasks
    let build_paths: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let build_errors: Arc<Mutex<HashMap<String, NixError>>> = Arc::new(Mutex::new(HashMap::new()));

    // Separate skipped targets (no build needed) from those requiring builds
    let mut dag_builder = DagBuilder::new();
    let flake_uri_owned = flake_uri.to_string();

    for target in &plan.targets {
        if !target.needs_build {
            // Record skipped targets immediately
            build_paths
                .lock()
                .unwrap()
                .insert(target.node.name.clone(), target.toplevel_path.clone());
            continue;
        }

        let hostname = target.node.name.clone();
        let flake_uri_clone = flake_uri_owned.clone();
        let machines_file_clone = machines_file.clone();
        let paths = Arc::clone(&build_paths);
        let errors = Arc::clone(&build_errors);

        // Create a task ID based on the hostname
        let task_id = format!("build:{}", hostname);

        // Create a closure that captures the build parameters
        let task = FnTask::new(format!("build {}", hostname), move |_ctx| match build_host(
            &flake_uri_clone,
            &hostname,
            machines_file_clone.as_deref(),
        ) {
            Ok(path) => {
                paths.lock().unwrap().insert(hostname.clone(), path);
                TaskOutcome::Success
            }
            Err(e) => {
                errors.lock().unwrap().insert(hostname.clone(), e);
                TaskOutcome::Failed(format!("build failed for {}", hostname))
            }
        });

        dag_builder.add_task(task_id, task);
    }

    // Set concurrency limit if specified
    if plan.max_parallel > 0 {
        dag_builder.concurrency_group("builds", plan.max_parallel);
        for target in &plan.targets {
            if target.needs_build {
                let task_id = format!("build:{}", target.node.name);
                dag_builder.assign_group(task_id, "builds");
            }
        }
    }

    // Execute the DAG
    let executor = dag_builder.build()?;
    let _report = executor.run()?;

    // Extract results from Arc<Mutex<_>>
    let final_paths = build_paths.lock().unwrap().clone();
    let final_errors = build_errors.lock().unwrap().clone();

    let results = BuildResults {
        paths: final_paths,
        errors: final_errors,
    };

    Ok(results)
}

/// Build any flake attribute and return its store path.
///
/// This is the generic build primitive — consortium-ansible uses it for
/// `ansibleEnvs.{name}`, consortium-slurm for `slurmEnvs.{name}`, etc.
pub fn build_flake_attr(flake_attr: &str, machines_file: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("nix");
    cmd.args(["build", flake_attr, "--no-link", "--print-out-paths"]);

    if let Some(path) = machines_file {
        cmd.arg("--builders").arg(format!("@{}", path));
    }

    let output = cmd.output().map_err(|e| NixError::BuildFailed {
        host: flake_attr.to_string(),
        message: format!("failed to run nix build: {}", e),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(NixError::BuildFailed {
            host: flake_attr.to_string(),
            message: stderr.to_string(),
        });
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(NixError::BuildFailed {
            host: flake_attr.to_string(),
            message: "nix build returned empty path".to_string(),
        });
    }

    Ok(path)
}

/// Build the system closure for a single host.
pub fn build_host(flake_uri: &str, hostname: &str, machines_file: Option<&str>) -> Result<String> {
    let attr = format!(
        "{}#nixosConfigurations.{}.config.system.build.toplevel",
        flake_uri, hostname
    );
    build_flake_attr(&attr, machines_file)
}

/// Generate a temporary machines file from healthy builders.
/// Public so the deploy pipeline can call it before building the DAG.
pub fn generate_machines_file_from_healthy(builders: &[HealthStatus]) -> Result<String> {
    let content: String = builders
        .iter()
        .filter(|b| b.healthy)
        .map(|b| {
            let key = b.builder.ssh_key.as_deref().unwrap_or("-");
            let features = b.builder.features.join(",");
            let systems = b.builder.systems.join(",");
            format!(
                "{}://{}@{} {} {} {} {} {}",
                b.builder.protocol,
                b.builder.user,
                b.builder.host,
                systems,
                key,
                b.builder.max_jobs,
                b.builder.speed_factor,
                features
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let dir = std::env::temp_dir().join("consortium-nix");
    std::fs::create_dir_all(&dir).map_err(|e| NixError::MachinesFile {
        path: dir.clone(),
        source: e,
    })?;

    let path = dir.join("machines");
    let mut file = std::fs::File::create(&path).map_err(|e| NixError::MachinesFile {
        path: path.clone(),
        source: e,
    })?;
    file.write_all(content.as_bytes())
        .map_err(|e| NixError::MachinesFile {
            path: path.clone(),
            source: e,
        })?;

    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Builder;

    #[test]
    fn test_generate_machines_file() {
        let builders = vec![HealthStatus {
            builder: Builder {
                host: "192.168.1.121".to_string(),
                user: "root".to_string(),
                max_jobs: 16,
                speed_factor: 2,
                systems: vec!["x86_64-linux".to_string()],
                features: vec!["big-parallel".to_string(), "kvm".to_string()],
                ssh_key: None,
                protocol: "ssh-ng".to_string(),
            },
            healthy: true,
            latency_ms: Some(5),
            error: None,
        }];

        let path = generate_machines_file_from_healthy(&builders).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("ssh-ng://root@192.168.1.121"));
        assert!(content.contains("x86_64-linux"));
        assert!(content.contains("big-parallel,kvm"));
        std::fs::remove_file(path).ok();
    }
}
