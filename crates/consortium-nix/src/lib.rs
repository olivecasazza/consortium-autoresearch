//! # consortium-nix
//!
//! NixOS deployment orchestration for consortium.
//!
//! This crate provides the deployment pipeline for NixOS and nix-darwin systems,
//! replacing colmena. It consumes fleet configuration (produced by the Nix library)
//! and orchestrates the evaluate → build → copy → activate pipeline using
//! consortium's DAG executor for maximum parallelism with per-host pipelining.
//!
//! ## Architecture
//!
//! The deployment pipeline has four stages, executed as a DAG:
//!
//! ```text
//! For each host:
//!   eval(host) → build(host) → copy(host) → activate(host)
//! ```
//!
//! Stages run in parallel across hosts (up to concurrency limits), and each
//! host can advance independently — host A can be copying while host B is
//! still building.
//!
//! Builder health checking ([`health`]) validates remote builders before use.

pub mod activate;
pub mod build;
pub mod cascade;
pub mod cascade_events;
pub mod cascade_executor;
pub mod cascade_integration;
pub mod cascade_strategies;
pub mod cascade_trace;
pub mod closure_introspect;
pub mod config;
pub mod copy;
pub mod error;
pub mod eval;
pub mod health;
pub mod tasks;

pub use config::{DeployAction, DeploymentNode, DeploymentPlan, FleetConfig, ProfileType};
pub use error::{NixError, Result};

use consortium::dag::{DagContext, DagReport, ErrorPolicy, StageBuilder, TaskId};

use crate::cascade_events::EventSink;
use crate::cascade_integration::{
    cascade_copy_grouped, cascade_copy_unified, CascadeCopyConfig, CascadeCopyTarget,
};
use crate::closure_introspect::shared_paths;
use crate::copy::parallel_diff_copy;

/// Run the full deployment pipeline using the DAG executor.
///
/// Each host progresses independently through eval → build → copy → activate,
/// with per-stage concurrency limits. A host that fails at any stage is
/// cancelled for subsequent stages without blocking other hosts.
pub fn deploy(
    config: &FleetConfig,
    target_nodes: &[String],
    action: DeployAction,
    max_parallel: usize,
    use_builders: bool,
) -> Result<DeployReport> {
    // Phase 0: Health check builders and prepare machines file
    let machines_file: Option<String> = if use_builders && !config.builders.is_empty() {
        let statuses = health::check_builders(config);
        let healthy: Vec<_> = statuses.iter().filter(|s| s.healthy).cloned().collect();
        if healthy.is_empty() {
            eprintln!("warning: no healthy builders available, building locally");
            None
        } else {
            match build::generate_machines_file_from_healthy(&healthy) {
                Ok(path) => Some(path),
                Err(e) => {
                    eprintln!("warning: failed to generate machines file: {}", e);
                    None
                }
            }
        }
    } else {
        None
    };

    // Set up shared context
    let ctx = DagContext::new();
    ctx.set_state("fleet_config", config.clone());
    ctx.set_state("action", action);
    if let Some(ref path) = machines_file {
        ctx.set_state("machines_file", path.clone());
    }

    // Determine stage concurrency limits
    // eval: limited to 1 (nix evaluation is memory-heavy)
    // build: up to max_parallel (nix distributes across builders internally)
    // copy: up to max_parallel (IO-bound, can be aggressive)
    // activate: limited to avoid overwhelming the fleet
    let eval_limit = 1;
    let build_limit = max_parallel;
    let copy_limit = max_parallel;
    let activate_limit = max_parallel.min(4);

    // Build the deployment DAG
    let mut builder = StageBuilder::new()
        .resources(target_nodes.to_vec())
        .stage("eval", Some(eval_limit), |host| {
            Box::new(tasks::NixEvalTask::new(host))
        })
        .stage("build", Some(build_limit), |host| {
            Box::new(tasks::NixBuildTask::new(host))
        })
        .error_policy(ErrorPolicy::ContinueIndependent)
        .context(ctx);

    // Only add copy + activate stages if not build-only
    if action != DeployAction::Build {
        builder = builder
            .stage("copy", Some(copy_limit), |host| {
                Box::new(tasks::NixCopyTask::new(host))
            })
            .stage("activate", Some(activate_limit), |host| {
                Box::new(tasks::NixActivateTask::new(host))
            });
    }

    let dag_report = builder
        .build()
        .map_err(|e| NixError::General(e.to_string()))?
        .run()
        .map_err(|e| NixError::General(e.to_string()))?;

    Ok(DeployReport::from_dag_report(
        &dag_report,
        target_nodes,
        action,
    ))
}

/// Cascade-driven deploy: same eval/build/activate as [`deploy`], but
/// the per-host `nix copy` stage is replaced by a single whole-fleet
/// cascade that distributes each toplevel peer-to-peer.
///
/// ## Why
///
/// `deploy()` runs N parallel `nix copy` subprocesses, each pulling
/// from the build host. The build host's uplink is the bottleneck —
/// at scale the deploy serializes on it.
///
/// `deploy_with_cascade()` lets nodes that have already received the
/// closure serve the next round's targets. With `fanout=2` and N hosts
/// sharing one toplevel, copy time drops from `N * single_copy_duration`
/// to `ceil(log2(N+1)) * single_copy_duration` in the best case.
///
/// ## How
///
/// 1. **DAG phase 1**: per-host eval + build (same as `deploy()`).
/// 2. **Cascade phase**: collect built toplevels, group by toplevel,
///    run one `cascade_copy_grouped()` per group. Live UI via
///    `event_sink` if provided.
/// 3. **DAG phase 2**: per-host activate (only for hosts whose copy
///    succeeded; failed-copy hosts are reported as copy_failures).
///
/// ## When NOT to use
///
/// - 1-2 targets: cascade overhead > parallel direct copy. Use `deploy()`.
/// - `action == DeployAction::Build`: nothing to copy. Use `deploy()`.
/// - First-time use against an untrusted fleet: prefer `deploy()` first
///   to validate SSH + signing trust path, then switch.
pub fn deploy_with_cascade(
    config: &FleetConfig,
    target_nodes: &[String],
    action: DeployAction,
    max_parallel: usize,
    use_builders: bool,
    cascade_fanout: u32,
    seed_addr: &str,
    event_sink: Option<&dyn EventSink>,
) -> Result<DeployReport> {
    // Build-only path: no copy, no cascade — defer to deploy().
    if action == DeployAction::Build {
        return deploy(config, target_nodes, action, max_parallel, use_builders);
    }

    // Phase 0: builder health check (same as deploy()).
    let machines_file: Option<String> = if use_builders && !config.builders.is_empty() {
        let statuses = health::check_builders(config);
        let healthy: Vec<_> = statuses.iter().filter(|s| s.healthy).cloned().collect();
        if healthy.is_empty() {
            eprintln!("warning: no healthy builders available, building locally");
            None
        } else {
            match build::generate_machines_file_from_healthy(&healthy) {
                Ok(path) => Some(path),
                Err(e) => {
                    eprintln!("warning: failed to generate machines file: {}", e);
                    None
                }
            }
        }
    } else {
        None
    };

    // Phase 1 DAG: eval + build only.
    let ctx1 = DagContext::new();
    ctx1.set_state("fleet_config", config.clone());
    ctx1.set_state("action", action);
    if let Some(ref path) = machines_file {
        ctx1.set_state("machines_file", path.clone());
    }
    let phase1_report = StageBuilder::new()
        .resources(target_nodes.to_vec())
        .stage("eval", Some(1), |host| {
            Box::new(tasks::NixEvalTask::new(host))
        })
        .stage("build", Some(max_parallel), |host| {
            Box::new(tasks::NixBuildTask::new(host))
        })
        .error_policy(ErrorPolicy::ContinueIndependent)
        .context(ctx1.clone())
        .build()
        .map_err(|e| NixError::General(e.to_string()))?
        .run()
        .map_err(|e| NixError::General(e.to_string()))?;

    // Collect successfully-built (host, toplevel) pairs from ctx1
    // outputs. Skip hosts whose build failed OR whose build was
    // cancelled because eval failed upstream.
    let mut targets_for_cascade: Vec<CascadeCopyTarget> = Vec::new();
    let mut build_failures: Vec<(String, String)> = Vec::new();
    for host in target_nodes {
        let build_id = TaskId(format!("build:{}", host));
        let eval_id = TaskId(format!("eval:{}", host));

        if let Some(err) = phase1_report.failed.get(&build_id) {
            build_failures.push((host.clone(), format!("build: {err}")));
            continue;
        }
        // ContinueIndependent cancels build when eval fails for the
        // same host. Surface the eval error rather than the misleading
        // "build succeeded but no output" path, which only applies if
        // the executor + context wiring is broken.
        if let Some(err) = phase1_report.failed.get(&eval_id) {
            build_failures.push((host.clone(), format!("eval (build cancelled): {err}")));
            continue;
        }
        if phase1_report.cancelled.contains(&build_id) {
            build_failures.push((
                host.clone(),
                "build cancelled (upstream stage failed)".into(),
            ));
            continue;
        }
        let Some(toplevel) = ctx1.get_output::<String>(&build_id) else {
            build_failures.push((
                host.clone(),
                "internal: build completed but DAG context held no toplevel output \
                 — likely a context-wiring bug, please report"
                    .into(),
            ));
            continue;
        };
        let Some(node) = config.nodes.get(host) else {
            build_failures.push((host.clone(), "host missing from fleet config".into()));
            continue;
        };
        targets_for_cascade.push(CascadeCopyTarget {
            host_name: host.clone(),
            ssh_addr: format!("{}@{}", node.target_user, node.target_host),
            toplevel_path: toplevel,
        });
    }

    // Cascade phase — TWO patterns running in sequence:
    //
    //   1. CLOSURE-INTERSECTION CASCADE (peer-to-peer fan-out tree):
    //      Compute the SET of store paths every host's toplevel
    //      depends on — the closure intersection. These paths are
    //      needed by every host regardless of its individual toplevel
    //      (shared nixpkgs, kernel, userspace). Cascade JUST those
    //      paths via the fan-out tree, sending zero host-specific
    //      bytes.
    //
    //   2. PER-HOST DIFF (parallel direct copy):
    //      Each host pulls its actual per-host toplevel directly
    //      from seed. nix's content-addressed store sees the shared
    //      substrate already on the host and only transfers the
    //      per-host tail. These run in parallel.
    //
    // Fallback: if closure introspection fails (nix not on PATH, or
    // toplevels not in the local store) we fall back to picking the
    // most-frequent toplevel as a "carrier" and cascading IT instead.
    // Slightly less efficient (sends carrier-specific bits to every
    // host) but still gets fan-out for the substrate that overlaps.
    let cascade_result;
    let mut copy_failures_extra: Vec<(String, String)> = Vec::new();

    if !targets_for_cascade.is_empty() {
        let toplevels: Vec<String> = targets_for_cascade
            .iter()
            .map(|t| t.toplevel_path.clone())
            .collect();

        match shared_paths(&toplevels) {
            Ok(paths) if !paths.is_empty() => {
                eprintln!(
                    "cascade: closure intersection = {} shared paths across {} toplevels",
                    paths.len(),
                    toplevels.len(),
                );
                cascade_result = cascade_copy_unified(
                    seed_addr,
                    &targets_for_cascade,
                    paths,
                    cascade_fanout,
                    std::time::Duration::from_secs(300),
                    event_sink,
                );
            }
            other => {
                if let Err(e) = other {
                    eprintln!(
                        "cascade: closure introspection failed ({e}); falling back to carrier toplevel"
                    );
                } else {
                    eprintln!(
                        "cascade: closure intersection empty; falling back to carrier toplevel"
                    );
                }
                let mut counts: std::collections::HashMap<&str, usize> =
                    std::collections::HashMap::new();
                for t in &targets_for_cascade {
                    *counts.entry(t.toplevel_path.as_str()).or_insert(0) += 1;
                }
                let carrier_toplevel: String = counts
                    .into_iter()
                    .max_by_key(|(_, n)| *n)
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_else(|| targets_for_cascade[0].toplevel_path.clone());

                let cascade_carrier_targets: Vec<CascadeCopyTarget> = targets_for_cascade
                    .iter()
                    .map(|t| CascadeCopyTarget {
                        host_name: t.host_name.clone(),
                        ssh_addr: t.ssh_addr.clone(),
                        toplevel_path: carrier_toplevel.clone(),
                    })
                    .collect();

                let mut cfg =
                    CascadeCopyConfig::new(seed_addr.to_string(), cascade_carrier_targets)
                        .fanout(cascade_fanout);
                if let Some(sink) = event_sink {
                    cfg = cfg.events(sink);
                }
                cascade_result = cascade_copy_grouped(cfg);
            }
        }

        // Per-host diff phase: every host that received the cascade
        // payload now needs its actual per-host toplevel pulled from
        // seed. nix copy only sends the missing paths (per-host tail)
        // since the shared substrate is already on the host. For
        // closure-intersection this means every host gets a diff
        // copy; for the carrier-toplevel fallback the host whose
        // toplevel == carrier already has its full closure but the
        // direct copy is a fast no-op there.
        let needs_diff: Vec<&CascadeCopyTarget> = targets_for_cascade
            .iter()
            .filter(|t| cascade_result.copied.iter().any(|h| h == &t.host_name))
            .collect();

        if !needs_diff.is_empty() {
            eprintln!(
                "cascade: shared-substrate done; {} host(s) need per-host diff copy",
                needs_diff.len()
            );
            let diff_results = parallel_diff_copy(&needs_diff);
            for (host, err) in diff_results {
                copy_failures_extra.push((host, err));
            }
        }
    } else {
        cascade_result = crate::cascade_integration::CascadeCopyResult::default();
    }

    // Build a synthetic Phase-2 DagContext that pre-loads the cascade
    // results into "copy:{host}" outputs so NixActivateTask can read
    // them without modification.
    let ctx2 = DagContext::new();
    ctx2.set_state("fleet_config", config.clone());
    ctx2.set_state("action", action);

    // Carry the toplevels forward so activate can find them. Hosts
    // whose carrier-cascade OR per-host diff failed are excluded —
    // they can't be activated (don't have their actual toplevel yet).
    let copied_set: std::collections::HashSet<&String> = cascade_result.copied.iter().collect();
    let diff_failed: std::collections::HashSet<&String> =
        copy_failures_extra.iter().map(|(h, _)| h).collect();
    let mut activate_targets: Vec<String> = Vec::new();
    for t in &targets_for_cascade {
        if copied_set.contains(&t.host_name) && !diff_failed.contains(&t.host_name) {
            ctx2.set_output(
                TaskId(format!("copy:{}", t.host_name)),
                t.toplevel_path.clone(),
            );
            activate_targets.push(t.host_name.clone());
        }
    }

    // Phase 2 DAG: activate only.
    let phase2_report = if !activate_targets.is_empty() {
        StageBuilder::new()
            .resources(activate_targets.clone())
            .stage("activate", Some(max_parallel.min(4)), |host| {
                Box::new(tasks::NixActivateTask::new(host))
            })
            .error_policy(ErrorPolicy::ContinueIndependent)
            .context(ctx2)
            .build()
            .map_err(|e| NixError::General(e.to_string()))?
            .run()
            .map_err(|e| NixError::General(e.to_string()))?
    } else {
        DagReport {
            completed: Default::default(),
            skipped: Default::default(),
            failed: Default::default(),
            cancelled: Default::default(),
        }
    };

    // Synthesize a DeployReport from the three phases.
    let mut activated = Vec::new();
    let mut activation_failures = Vec::new();
    for host in &activate_targets {
        let id = TaskId(format!("activate:{}", host));
        if phase2_report.completed.contains(&id) || phase2_report.skipped.contains(&id) {
            activated.push(host.clone());
        } else if let Some(err) = phase2_report.failed.get(&id) {
            activation_failures.push((host.clone(), err.clone()));
        }
    }

    let built: Vec<String> = target_nodes
        .iter()
        .filter(|h| !build_failures.iter().any(|(b, _)| &b == h))
        .cloned()
        .collect();

    let mut copy_failures: Vec<(String, String)> = cascade_result
        .failed
        .into_iter()
        .map(|(h, e)| (h, e))
        .collect();
    // Fold in per-host diff-copy failures from the second cascade phase.
    copy_failures.extend(copy_failures_extra);

    // Defensive: any host that built successfully but is in neither
    // `copied` nor `failed` was silently dropped by the cascade
    // (a strategy bug — e.g. dense-id assumption violated, or a
    // group that converged without attempting its target). Mark
    // them as failures explicitly so the report is honest.
    let copied_set: std::collections::HashSet<String> =
        cascade_result.copied.iter().cloned().collect();
    let failed_set: std::collections::HashSet<String> =
        copy_failures.iter().map(|(h, _)| h.clone()).collect();
    for t in &targets_for_cascade {
        if !copied_set.contains(&t.host_name) && !failed_set.contains(&t.host_name) {
            copy_failures.push((
                t.host_name.clone(),
                "cascade did not attempt this host (strategy halted without planning the edge)"
                    .to_string(),
            ));
        }
    }

    Ok(DeployReport {
        built,
        copied: cascade_result.copied,
        activated,
        build_failures,
        copy_failures,
        activation_failures,
    })
}

/// Summary of a deployment run.
#[derive(Debug)]
pub struct DeployReport {
    /// Hosts whose closures were built successfully.
    pub built: Vec<String>,
    /// Hosts whose closures were copied successfully.
    pub copied: Vec<String>,
    /// Hosts that were activated successfully.
    pub activated: Vec<String>,
    /// Hosts that failed to build (name, error message).
    pub build_failures: Vec<(String, String)>,
    /// Hosts that failed closure copy (name, error message).
    pub copy_failures: Vec<(String, String)>,
    /// Hosts that failed activation (name, error message).
    pub activation_failures: Vec<(String, String)>,
}

impl DeployReport {
    /// Whether the deployment was fully successful (no failures).
    pub fn is_success(&self) -> bool {
        self.build_failures.is_empty()
            && self.copy_failures.is_empty()
            && self.activation_failures.is_empty()
    }

    /// Total number of failures across all phases.
    pub fn failure_count(&self) -> usize {
        self.build_failures.len() + self.copy_failures.len() + self.activation_failures.len()
    }

    /// Number of hosts that completed successfully (all phases).
    pub fn success_count(&self) -> usize {
        self.activated.len()
    }

    /// Build a DeployReport from a DagReport by inspecting task IDs.
    fn from_dag_report(report: &DagReport, target_nodes: &[String], action: DeployAction) -> Self {
        let mut built = Vec::new();
        let mut copied = Vec::new();
        let mut activated = Vec::new();
        let mut build_failures = Vec::new();
        let mut copy_failures = Vec::new();
        let mut activation_failures = Vec::new();

        for host in target_nodes {
            let build_id = consortium::dag::TaskId(format!("build:{}", host));
            let copy_id = consortium::dag::TaskId(format!("copy:{}", host));
            let activate_id = consortium::dag::TaskId(format!("activate:{}", host));

            // Check build
            if report.completed.contains(&build_id) || report.skipped.contains(&build_id) {
                built.push(host.clone());
            } else if let Some(err) = report.failed.get(&build_id) {
                build_failures.push((host.clone(), err.clone()));
            }
            // cancelled builds are not reported as failures — they're implied by an earlier failure

            if action == DeployAction::Build {
                continue;
            }

            // Check copy
            if report.completed.contains(&copy_id) || report.skipped.contains(&copy_id) {
                copied.push(host.clone());
            } else if let Some(err) = report.failed.get(&copy_id) {
                copy_failures.push((host.clone(), err.clone()));
            }

            // Check activate
            if report.completed.contains(&activate_id) || report.skipped.contains(&activate_id) {
                activated.push(host.clone());
            } else if let Some(err) = report.failed.get(&activate_id) {
                activation_failures.push((host.clone(), err.clone()));
            }
        }

        Self {
            built,
            copied,
            activated,
            build_failures,
            copy_failures,
            activation_failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use consortium::dag::{DagReport, TaskId};
    use std::collections::HashSet;

    fn make_report(completed: &[&str], failed: &[(&str, &str)], cancelled: &[&str]) -> DagReport {
        DagReport {
            completed: completed.iter().map(|s| TaskId(s.to_string())).collect(),
            skipped: HashSet::new(),
            failed: failed
                .iter()
                .map(|(k, v)| (TaskId(k.to_string()), v.to_string()))
                .collect(),
            cancelled: cancelled.iter().map(|s| TaskId(s.to_string())).collect(),
        }
    }

    #[test]
    fn test_deploy_report_all_success() {
        let dag_report = make_report(
            &[
                "eval:hp01",
                "build:hp01",
                "copy:hp01",
                "activate:hp01",
                "eval:hp02",
                "build:hp02",
                "copy:hp02",
                "activate:hp02",
            ],
            &[],
            &[],
        );
        let targets = vec!["hp01".to_string(), "hp02".to_string()];
        let report = DeployReport::from_dag_report(&dag_report, &targets, DeployAction::Switch);

        assert!(report.is_success());
        assert_eq!(report.built, vec!["hp01", "hp02"]);
        assert_eq!(report.copied, vec!["hp01", "hp02"]);
        assert_eq!(report.activated, vec!["hp01", "hp02"]);
        assert_eq!(report.failure_count(), 0);
    }

    #[test]
    fn test_deploy_report_build_failure() {
        let dag_report = make_report(
            &[
                "eval:hp01",
                "eval:hp02",
                "build:hp01",
                "copy:hp01",
                "activate:hp01",
            ],
            &[("build:hp02", "nix build failed")],
            &["copy:hp02", "activate:hp02"],
        );
        let targets = vec!["hp01".to_string(), "hp02".to_string()];
        let report = DeployReport::from_dag_report(&dag_report, &targets, DeployAction::Switch);

        assert!(!report.is_success());
        assert_eq!(report.built, vec!["hp01"]);
        assert_eq!(report.activated, vec!["hp01"]);
        assert_eq!(
            report.build_failures,
            vec![("hp02".to_string(), "nix build failed".to_string())]
        );
        // copy and activate for hp02 are cancelled, not failed
        assert!(report.copy_failures.is_empty());
        assert!(report.activation_failures.is_empty());
    }

    #[test]
    fn test_deploy_report_copy_failure() {
        let dag_report = make_report(
            &["eval:hp01", "build:hp01", "eval:hp02", "build:hp02"],
            &[("copy:hp01", "ssh connection refused")],
            &["activate:hp01"],
        );
        let targets = vec!["hp01".to_string(), "hp02".to_string()];
        let report = DeployReport::from_dag_report(&dag_report, &targets, DeployAction::Switch);

        assert!(!report.is_success());
        assert_eq!(report.built, vec!["hp01", "hp02"]); // both built
        assert_eq!(
            report.copy_failures,
            vec![("hp01".to_string(), "ssh connection refused".to_string())]
        );
    }

    #[test]
    fn test_deploy_report_build_only() {
        let dag_report = make_report(
            &["eval:hp01", "build:hp01", "eval:hp02", "build:hp02"],
            &[],
            &[],
        );
        let targets = vec!["hp01".to_string(), "hp02".to_string()];
        let report = DeployReport::from_dag_report(&dag_report, &targets, DeployAction::Build);

        assert!(report.is_success());
        assert_eq!(report.built, vec!["hp01", "hp02"]);
        assert!(report.copied.is_empty());
        assert!(report.activated.is_empty());
    }

    #[test]
    fn test_deploy_action_display_roundtrip() {
        for action in &["switch", "boot", "test", "dry-activate", "build"] {
            let parsed: DeployAction = action.parse().unwrap();
            assert_eq!(parsed.to_string(), *action);
        }
    }

    #[test]
    fn test_deploy_action_invalid() {
        assert!("reboot".parse::<DeployAction>().is_err());
        assert!("".parse::<DeployAction>().is_err());
        assert!("SWITCH".parse::<DeployAction>().is_err());
    }
}
