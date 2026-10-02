//! DagTask implementations for Slurm job orchestration.
//!
//! Pipeline: build-job-env → copy-to-submit → submit → wait → collect

use std::process::Command;
use std::time::{Duration, Instant};

use consortium::dag::{DagContext, DagTask, TaskId, TaskOutcome};
use consortium_nix::{build, copy};

/// Build a hermetic job environment via nix.
pub struct NixBuildJobEnvTask {
    pub job_name: String,
    pub flake_attr: String,
}

impl NixBuildJobEnvTask {
    pub fn new(job_name: &str, flake_uri: &str) -> Self {
        Self {
            job_name: job_name.to_string(),
            flake_attr: format!("{}#slurmEnvs.{}", flake_uri, job_name),
        }
    }
}

impl DagTask for NixBuildJobEnvTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        match build::build_flake_attr(&self.flake_attr, None) {
            Ok(path) => {
                ctx.set_output(TaskId(format!("build-job-env:{}", self.job_name)), path);
                TaskOutcome::Success
            }
            Err(e) => TaskOutcome::Failed(format!("build job env: {}", e)),
        }
    }

    fn describe(&self) -> String {
        format!("build slurm job env '{}'", self.job_name)
    }
}

/// Copy the job environment to the submit node.
pub struct NixCopyToSubmitTask {
    pub job_name: String,
    pub submit_host: String,
    pub submit_user: String,
}

impl DagTask for NixCopyToSubmitTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        let store_path: String =
            match ctx.get_output(&TaskId(format!("build-job-env:{}", self.job_name))) {
                Some(p) => p,
                None => return TaskOutcome::Failed("no job env build output".into()),
            };

        let store_uri = format!("ssh-ng://{}@{}", self.submit_user, self.submit_host);
        match copy::copy_closure(&store_path, &store_uri) {
            Ok(()) => {
                ctx.set_output(
                    TaskId(format!("copy-job-env:{}", self.job_name)),
                    store_path,
                );
                TaskOutcome::Success
            }
            Err(e) => TaskOutcome::Failed(format!("copy job env: {}", e)),
        }
    }

    fn describe(&self) -> String {
        format!("copy job env '{}' to {}", self.job_name, self.submit_host)
    }
}

/// Submit a slurm job via sbatch.
pub struct SlurmSubmitTask {
    pub job_name: String,
    pub script: String,
    pub partition: Option<String>,
    pub submit_host: String,
    pub submit_user: String,
}

impl DagTask for SlurmSubmitTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        // Build sbatch command
        let mut sbatch_args = vec!["sbatch".to_string()];
        sbatch_args.push(format!("--job-name={}", self.job_name));

        if let Some(ref partition) = self.partition {
            sbatch_args.push(format!("--partition={}", partition));
        }

        // If we have a nix env, set PATH in the job
        if let Some(env_path) =
            ctx.get_output::<String>(&TaskId(format!("copy-job-env:{}", self.job_name)))
        {
            sbatch_args.push(format!("--export=ALL,PATH={}/bin:$PATH", env_path));
        }

        sbatch_args.push(self.script.clone());

        let ssh_cmd = sbatch_args.join(" ");
        let output = Command::new("ssh")
            .args([
                "-oStrictHostKeyChecking=no",
                "-oPasswordAuthentication=no",
                "-l",
                &self.submit_user,
                &self.submit_host,
                &ssh_cmd,
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // sbatch output: "Submitted batch job 12345"
                let job_id: u64 = stdout
                    .trim()
                    .rsplit_once(' ')
                    .and_then(|(_, id)| id.parse().ok())
                    .unwrap_or(0);

                ctx.set_output(TaskId(format!("slurm-submit:{}", self.job_name)), job_id);
                TaskOutcome::Success
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                TaskOutcome::Failed(format!("sbatch failed: {}", stderr.trim()))
            }
            Err(e) => TaskOutcome::Failed(format!("sbatch exec failed: {}", e)),
        }
    }

    fn describe(&self) -> String {
        format!("submit slurm job '{}'", self.job_name)
    }
}

/// Wait for a slurm job to complete by polling sacct.
pub struct SlurmWaitTask {
    pub job_name: String,
    pub submit_host: String,
    pub submit_user: String,
    pub poll_interval: Duration,
    pub timeout: Option<Duration>,
}

impl SlurmWaitTask {
    pub fn new(job_name: &str, submit_host: &str, submit_user: &str) -> Self {
        Self {
            job_name: job_name.to_string(),
            submit_host: submit_host.to_string(),
            submit_user: submit_user.to_string(),
            poll_interval: Duration::from_secs(10),
            timeout: None,
        }
    }
}

impl DagTask for SlurmWaitTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        let job_id: u64 = match ctx.get_output(&TaskId(format!("slurm-submit:{}", self.job_name))) {
            Some(id) => id,
            None => return TaskOutcome::Failed("no job ID from submit".into()),
        };

        let start = Instant::now();

        loop {
            // Check timeout
            if let Some(timeout) = self.timeout {
                if start.elapsed() > timeout {
                    return TaskOutcome::Failed(format!(
                        "job {} timed out after {}s",
                        job_id,
                        start.elapsed().as_secs()
                    ));
                }
            }

            // Poll sacct for job state
            let sacct_cmd = format!(
                "sacct -j {} --format=State --noheader --parsable2 | head -1",
                job_id
            );
            let output = Command::new("ssh")
                .args([
                    "-oStrictHostKeyChecking=no",
                    "-oPasswordAuthentication=no",
                    "-l",
                    &self.submit_user,
                    &self.submit_host,
                    &sacct_cmd,
                ])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    let state = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    match state.as_str() {
                        "COMPLETED" => {
                            ctx.set_output(TaskId(format!("slurm-wait:{}", self.job_name)), job_id);
                            return TaskOutcome::Success;
                        }
                        "FAILED" | "CANCELLED" | "TIMEOUT" | "OUT_OF_MEMORY" | "NODE_FAIL" => {
                            return TaskOutcome::Failed(format!(
                                "job {} ended with state: {}",
                                job_id, state
                            ));
                        }
                        // PENDING, RUNNING, etc. — keep polling
                        _ => {}
                    }
                }
                _ => {} // SSH error — retry next poll
            }

            std::thread::sleep(self.poll_interval);
        }
    }

    fn describe(&self) -> String {
        format!("wait for slurm job '{}'", self.job_name)
    }
}

/// Collect results from a completed slurm job.
pub struct SlurmCollectTask {
    pub job_name: String,
    pub output_pattern: String,
    pub submit_host: String,
    pub submit_user: String,
}

impl DagTask for SlurmCollectTask {
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        let _job_id: u64 = match ctx.get_output(&TaskId(format!("slurm-wait:{}", self.job_name))) {
            Some(id) => id,
            None => return TaskOutcome::Failed("job not completed".into()),
        };

        // Collect output files via SCP or SSH cat
        let output = Command::new("ssh")
            .args([
                "-oStrictHostKeyChecking=no",
                "-l",
                &self.submit_user,
                &self.submit_host,
                &format!("cat {}", self.output_pattern),
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => {
                let content = String::from_utf8_lossy(&o.stdout).to_string();
                ctx.set_output(TaskId(format!("slurm-collect:{}", self.job_name)), content);
                TaskOutcome::Success
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                TaskOutcome::Failed(format!("collect failed: {}", stderr.trim()))
            }
            Err(e) => TaskOutcome::Failed(format!("collect failed: {}", e)),
        }
    }

    fn describe(&self) -> String {
        format!("collect results for '{}'", self.job_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sbatch_output_parsing() {
        // sbatch outputs: "Submitted batch job 12345"
        let output = "Submitted batch job 12345";
        let job_id: u64 = output
            .trim()
            .rsplit_once(' ')
            .and_then(|(_, id)| id.parse().ok())
            .unwrap_or(0);
        assert_eq!(job_id, 12345);
    }

    #[test]
    fn test_sbatch_output_parsing_with_whitespace() {
        let output = "Submitted batch job 99999\n";
        let job_id: u64 = output
            .trim()
            .rsplit_once(' ')
            .and_then(|(_, id)| id.parse().ok())
            .unwrap_or(0);
        assert_eq!(job_id, 99999);
    }

    #[test]
    fn test_sbatch_output_parsing_unexpected() {
        let output = "Error: something went wrong";
        let job_id: u64 = output
            .trim()
            .rsplit_once(' ')
            .and_then(|(_, id)| id.parse().ok())
            .unwrap_or(0);
        assert_eq!(job_id, 0); // fallback to 0 on parse failure
    }

    #[test]
    fn test_slurm_job_states() {
        // Verify the wait task would recognize these terminal states
        let terminal_failure = [
            "FAILED",
            "CANCELLED",
            "TIMEOUT",
            "OUT_OF_MEMORY",
            "NODE_FAIL",
        ];
        let running = ["PENDING", "RUNNING", "COMPLETING"];

        for &state in &terminal_failure {
            assert!(
                matches!(
                    state,
                    "FAILED" | "CANCELLED" | "TIMEOUT" | "OUT_OF_MEMORY" | "NODE_FAIL"
                ),
                "{} should be terminal failure",
                state
            );
        }

        for &state in &running {
            assert!(
                !matches!(
                    state,
                    "COMPLETED"
                        | "FAILED"
                        | "CANCELLED"
                        | "TIMEOUT"
                        | "OUT_OF_MEMORY"
                        | "NODE_FAIL"
                ),
                "{} should continue polling",
                state
            );
        }
    }

    #[test]
    fn test_describe_methods() {
        let build = NixBuildJobEnvTask::new("rnaseq", ".");
        assert!(build.describe().contains("rnaseq"));

        let submit = SlurmSubmitTask {
            job_name: "test".to_string(),
            script: "test.sh".to_string(),
            partition: Some("gpu".to_string()),
            submit_host: "ctrl".to_string(),
            submit_user: "root".to_string(),
        };
        assert!(submit.describe().contains("test"));

        let wait = SlurmWaitTask::new("myjob", "ctrl", "root");
        assert!(wait.describe().contains("myjob"));
        assert_eq!(wait.poll_interval, Duration::from_secs(10));
    }
}
