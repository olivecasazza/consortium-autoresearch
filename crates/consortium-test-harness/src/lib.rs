//! Docker-based mini-HPC test harness for consortium.
//!
//! Provides a `DockerCluster` that manages a fleet of Alpine SSH containers
//! for integration testing. The cluster is started once per test binary via
//! `LazyLock` and torn down on exit.
//!
//! # Usage
//!
//! ```rust,ignore
//! use consortium_test_harness::DockerCluster;
//! use std::sync::LazyLock;
//!
//! static CLUSTER: LazyLock<DockerCluster> = LazyLock::new(|| {
//!     DockerCluster::start_default().expect("docker cluster failed")
//! });
//!
//! #[test]
//! fn test_ssh_to_node() {
//!     let cluster = &*CLUSTER;
//!     let opts = cluster.ssh_options();
//!     // ... use SshWorker with opts
//! }
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use consortium::worker::ssh::SshOptions;
use consortium_nix::config::{DeploymentNode, FleetConfig, ProfileType};

pub mod netem;

pub use netem::{delay_from_duration, rate_from_bytes_per_sec, NetemProfile, DEFAULT_IFACE};

/// Cluster topology configuration.
#[derive(Debug, Clone)]
pub struct ClusterTopology {
    pub compute_count: usize,
    pub gpu_count: usize,
    pub login_count: usize,
    pub controller: bool,
}

impl Default for ClusterTopology {
    fn default() -> Self {
        Self {
            compute_count: 25,
            gpu_count: 5,
            login_count: 2,
            controller: true,
        }
    }
}

/// A running Docker Compose cluster for integration testing.
pub struct DockerCluster {
    compose_file: PathBuf,
    project_name: String,
    docker_dir: PathBuf,
    ssh_key_path: PathBuf,
    /// Map of node name → host port (for SSH access from host).
    node_ports: HashMap<String, u16>,
    topology: ClusterTopology,
}

/// Base port for SSH port mapping. compute-01 gets 2201, etc.
const BASE_PORT: u16 = 2200;

impl DockerCluster {
    /// Start a cluster with the default topology (25 compute + 5 GPU + 2 login + 1 controller).
    pub fn start_default() -> Result<Self, String> {
        Self::start(ClusterTopology::default())
    }

    /// Start a cluster with a small topology for quick tests.
    pub fn start_small() -> Result<Self, String> {
        Self::start(ClusterTopology {
            compute_count: 5,
            gpu_count: 0,
            login_count: 1,
            controller: false,
        })
    }

    /// Start a cluster with the given topology.
    pub fn start(topology: ClusterTopology) -> Result<Self, String> {
        // Clean up any stale containers from previous runs
        Self::cleanup_stale();

        // Check Docker is available
        let docker_check = Command::new("docker")
            .arg("info")
            .output()
            .map_err(|e| format!("docker not found: {}", e))?;
        if !docker_check.status.success() {
            return Err("docker daemon not running".into());
        }

        let docker_dir = Self::docker_dir();
        let project_name = format!("consortium-test-{}", std::process::id());

        // Generate SSH keys
        let ssh_key_path = Self::generate_ssh_keys(&docker_dir)?;

        // Generate compose file
        let (compose_file, node_ports) =
            Self::generate_compose(&docker_dir, &topology, &project_name)?;

        let cluster = Self {
            compose_file,
            project_name,
            docker_dir,
            ssh_key_path,
            node_ports,
            topology,
        };

        // Start containers
        cluster.compose_up()?;

        // Wait for all nodes to be ready
        cluster.wait_ready(Duration::from_secs(120))?;

        Ok(cluster)
    }

    /// Stop the cluster and remove containers.
    pub fn stop(&self) -> Result<(), String> {
        let output = Command::new("docker")
            .args([
                "compose",
                "-f",
                self.compose_file.to_str().unwrap(),
                "-p",
                &self.project_name,
                "down",
                "-v",
                "--remove-orphans",
            ])
            .output()
            .map_err(|e| format!("docker compose down failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("compose down failed: {}", stderr));
        }

        Ok(())
    }

    /// Get the SSH identity file path.
    pub fn ssh_key_path(&self) -> &Path {
        &self.ssh_key_path
    }

    /// Get SshOptions configured for this cluster.
    pub fn ssh_options(&self) -> SshOptions {
        SshOptions {
            identity_file: Some(self.ssh_key_path.to_string_lossy().to_string()),
            strict_host_key_checking: false,
            password_auth: false,
            connect_timeout: Some(5),
            ..SshOptions::default()
        }
    }

    /// Get SshOptions for a specific node (includes port).
    pub fn ssh_options_for(&self, node: &str) -> Option<SshOptions> {
        let port = self.node_ports.get(node)?;
        Some(SshOptions {
            identity_file: Some(self.ssh_key_path.to_string_lossy().to_string()),
            port: Some(*port),
            strict_host_key_checking: false,
            password_auth: false,
            connect_timeout: Some(5),
            ..SshOptions::default()
        })
    }

    /// Get the host port for a node.
    pub fn port_for(&self, node: &str) -> Option<u16> {
        self.node_ports.get(node).copied()
    }

    /// Get all node names.
    pub fn node_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.node_ports.keys().cloned().collect();
        names.sort();
        names
    }

    /// Get node names matching a prefix (e.g. "compute" returns compute-01..compute-25).
    pub fn nodes_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<_> = self
            .node_ports
            .keys()
            .filter(|n| n.starts_with(prefix))
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Generate a FleetConfig for this cluster.
    pub fn fleet_config(&self) -> FleetConfig {
        let mut nodes = HashMap::new();
        for (name, port) in &self.node_ports {
            let tags = Self::infer_tags(name);
            nodes.insert(
                name.clone(),
                DeploymentNode {
                    name: name.clone(),
                    target_host: "127.0.0.1".to_string(),
                    target_user: "root".to_string(),
                    target_port: Some(*port),
                    system: "x86_64-linux".to_string(),
                    profile_type: ProfileType::Nixos,
                    build_on_target: false,
                    tags,
                    drv_path: None,
                    toplevel: None,
                },
            );
        }
        FleetConfig {
            nodes,
            builders: HashMap::new(),
            flake_uri: ".".to_string(),
            ansible_config: None,
            slurm_config: None,
            ray_config: None,
            skypilot_config: None,
        }
    }

    /// Total number of nodes.
    pub fn node_count(&self) -> usize {
        self.node_ports.len()
    }

    // ─── Network shaping ───────────────────────────────────────────────────

    /// Run a program inside one node and return its stdout.
    ///
    /// Addressed through `docker compose exec` rather than by guessing container
    /// names, so the compose project the cluster started is the one that is
    /// addressed.
    ///
    /// This is the primitive the shaping assertions are built on: measuring
    /// whether a delay *took effect* means reading a clock on the node, not
    /// trusting that `tc` exited 0.
    pub fn run_in_node(&self, node: &str, program: &str, args: &[&str]) -> Result<String, String> {
        if !self.node_ports.contains_key(node) {
            return Err(format!("unknown node {node} in this cluster"));
        }
        let compose_file = self
            .compose_file
            .to_str()
            .ok_or_else(|| "compose file path is not valid UTF-8".to_string())?;

        let output = Command::new("docker")
            .args([
                "compose",
                "-f",
                compose_file,
                "-p",
                &self.project_name,
                "exec",
                "-T",
                node,
                program,
            ])
            .args(args)
            .output()
            .map_err(|e| format!("docker compose exec {node} {program} failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{program} on {node} failed: {stderr}"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Impose a `netem` profile on one node's network interface.
    ///
    /// This is the container-side half of a sim↔container calibration: the sim
    /// models bandwidth + latency per edge, and without shaping here the two
    /// tiers are not comparable (ADR 0001 §7.1).
    ///
    /// Rejects an empty profile rather than running a `tc` command that exits 0
    /// and shapes nothing — see [`netem`] for why that no-op matters.
    ///
    /// Requires `CAP_NET_ADMIN` in the node, which `plan_compose` grants to
    /// every generated service.
    pub fn apply_netem(
        &self,
        node: &str,
        profile: &NetemProfile,
        iface: &str,
    ) -> Result<(), String> {
        let args = profile.qdisc_args(iface)?;
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run_in_node(node, "tc", &borrowed).map(|_| ())
    }

    /// Remove shaping from one node's interface, restoring the kernel default
    /// qdisc.
    ///
    /// Idempotent: an already-unshaped interface reports no such qdisc, which
    /// this treats as success. Without that, teardown would fail on any node a
    /// test never shaped and shaping would leak between tests.
    pub fn clear_netem(&self, node: &str, iface: &str) -> Result<(), String> {
        let args = NetemProfile::clear_args(iface);
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.run_in_node(node, "tc", &borrowed) {
            Ok(_) => Ok(()),
            Err(err) => {
                if is_absent_qdisc(&err) {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Current root qdisc on one node's interface, as `tc qdisc show` reports it.
    ///
    /// Useful for asserting *which* profile is installed. Note that reading this
    /// back only proves `tc` was asked to apply the profile; asserting that
    /// shaping actually took effect means measuring timing, not parsing this.
    pub fn qdisc_show(&self, node: &str, iface: &str) -> Result<String, String> {
        self.run_in_node(node, "tc", &["qdisc", "show", "dev", iface])
    }

    // ─── Internal ────────────────────────────────────────────────────────

    /// Clean up any stale consortium test containers from previous runs.
    fn cleanup_stale() {
        // Remove containers
        let _ = Command::new("sh")
            .args([
                "-c",
                "docker ps -a --filter 'name=consortium-test' -q | xargs -r docker rm -f 2>/dev/null",
            ])
            .output();
        // Remove networks
        let _ = Command::new("sh")
            .args([
                "-c",
                "docker network ls --filter 'name=consortium-test' -q | xargs -r docker network rm 2>/dev/null",
            ])
            .output();
    }

    fn docker_dir() -> PathBuf {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(manifest)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tests")
            .join("docker")
    }

    fn generate_ssh_keys(docker_dir: &Path) -> Result<PathBuf, String> {
        let ssh_dir = docker_dir.join("ssh");
        std::fs::create_dir_all(&ssh_dir)
            .map_err(|e| format!("failed to create ssh dir: {}", e))?;

        let key_path = ssh_dir.join("id_ed25519");
        if !key_path.exists() {
            let output = Command::new("ssh-keygen")
                .args([
                    "-t",
                    "ed25519",
                    "-f",
                    key_path.to_str().unwrap(),
                    "-N",
                    "",
                    "-q",
                ])
                .output()
                .map_err(|e| format!("ssh-keygen failed: {}", e))?;

            if !output.status.success() {
                return Err("ssh-keygen failed".into());
            }
        }

        // Copy pub key to authorized_keys
        let pub_key = std::fs::read_to_string(ssh_dir.join("id_ed25519.pub"))
            .map_err(|e| format!("read pub key: {}", e))?;
        std::fs::write(ssh_dir.join("authorized_keys"), &pub_key)
            .map_err(|e| format!("write authorized_keys: {}", e))?;

        Ok(key_path)
    }

    fn generate_compose(
        docker_dir: &Path,
        topology: &ClusterTopology,
        _project_name: &str,
    ) -> Result<(PathBuf, HashMap<String, u16>), String> {
        let (yaml, ports) = plan_compose(topology);

        let compose_path = docker_dir.join("docker-compose.generated.yml");
        let mut f = std::fs::File::create(&compose_path)
            .map_err(|e| format!("create compose file: {}", e))?;
        f.write_all(yaml.as_bytes())
            .map_err(|e| format!("write compose file: {}", e))?;

        Ok((compose_path, ports))
    }

    fn compose_up(&self) -> Result<(), String> {
        let output = Command::new("docker")
            .args([
                "compose",
                "-f",
                self.compose_file.to_str().unwrap(),
                "-p",
                &self.project_name,
                "up",
                "-d",
                "--build",
                "--wait",
            ])
            .output()
            .map_err(|e| format!("docker compose up failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("compose up failed: {}", stderr));
        }

        Ok(())
    }

    fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let start = Instant::now();

        for (name, port) in &self.node_ports {
            loop {
                if start.elapsed() > timeout {
                    return Err(format!("timeout waiting for {} (port {})", name, port));
                }

                // Try SSH connection
                let result = Command::new("ssh")
                    .args([
                        "-oStrictHostKeyChecking=no",
                        "-oPasswordAuthentication=no",
                        "-oConnectTimeout=2",
                        "-oBatchMode=yes",
                        "-i",
                        self.ssh_key_path.to_str().unwrap(),
                        "-p",
                        &port.to_string(),
                        "root@127.0.0.1",
                        "true",
                    ])
                    .output();

                if let Ok(o) = result {
                    if o.status.success() {
                        break;
                    }
                }

                std::thread::sleep(Duration::from_millis(500));
            }
        }

        Ok(())
    }

    fn infer_tags(name: &str) -> Vec<String> {
        let mut tags = Vec::new();
        if name.starts_with("compute") {
            tags.push("compute".to_string());
        }
        if name.starts_with("gpu") {
            tags.push("gpu".to_string());
            tags.push("compute".to_string());
        }
        if name.starts_with("login") {
            tags.push("login".to_string());
        }
        if name == "controller" {
            tags.push("controller".to_string());
        }
        tags
    }
}

/// The compose file every SSH node is generated from, and the node → host port
/// map that goes with it.
///
/// Pure: no Docker, no filesystem, no I/O. Everything that can be checked
/// without a running daemon is checked here by unit test, because a compose
/// file that is subtly wrong fails much later and much less legibly.
fn plan_compose(topology: &ClusterTopology) -> (String, HashMap<String, u16>) {
    let mut services = Vec::new();
    let mut ports = HashMap::new();
    let mut port = BASE_PORT + 1;

    // CAP_NET_ADMIN is required for `tc qdisc` (see `netem`). Docker's default
    // capability set does *not* include it — the container's root user is
    // unprivileged with respect to its own network namespace — so without this
    // every `tc` call fails with EPERM and the shaping never happens.
    let anchor = r#"x-ssh-node: &ssh-node
  build:
    context: .
    dockerfile: Dockerfile.ssh-node
  volumes:
    - ./ssh/authorized_keys:/root/.ssh/authorized_keys:ro
  networks:
    - cluster
  cap_add:
    - NET_ADMIN
  restart: "no""#;

    let mut push = |name: &str| {
        services.push(format!(
            "  {}:\n    <<: *ssh-node\n    hostname: {}\n    ports:\n      - \"{}:22\"",
            name, name, port
        ));
        ports.insert(name.to_string(), port);
        port += 1;
    };

    for i in 1..=topology.compute_count {
        push(&format!("compute-{:02}", i));
    }

    for i in 1..=topology.gpu_count {
        push(&format!("gpu-{:02}", i));
    }

    for i in 1..=topology.login_count {
        push(&format!("login-{:02}", i));
    }

    if topology.controller {
        push("controller");
    }

    let yaml = format!(
        "{}\n\nservices:\n{}\n\nnetworks:\n  cluster:\n    driver: bridge\n",
        anchor,
        services.join("\n\n")
    );

    (yaml, ports)
}

impl Drop for DockerCluster {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Whether a `tc` failure means "there was nothing to delete" rather than a
/// real error.
///
/// `tc qdisc del` on an unshaped interface fails with `RTNETLINK answers: No
/// such file or directory` (and on some kernels `Cannot find specified qdisc`).
/// Teardown must treat that as already-clear, otherwise clearing shaping is
/// not idempotent and a node a test never shaped fails the teardown.
fn is_absent_qdisc(err: &str) -> bool {
    err.contains("No such file or directory") || err.contains("Cannot find specified qdisc")
}

/// Check if Docker is available. Returns false if docker is not installed
/// or the daemon is not running.
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod netem_teardown_tests {
    use super::is_absent_qdisc;

    #[test]
    fn absent_qdisc_messages_are_treated_as_already_clear() {
        // The exact stderr `tc qdisc del` produces on an unshaped interface.
        assert!(is_absent_qdisc(
            "tc on compute-01 failed: RTNETLINK answers: No such file or directory"
        ));
        assert!(is_absent_qdisc(
            "tc on compute-01 failed: Error: Cannot find specified qdisc."
        ));
    }

    #[test]
    fn real_tc_failures_are_not_swallowed() {
        // If these were treated as "already clear", a genuine shaping failure
        // would be silently ignored and the next test would run unshaped.
        assert!(!is_absent_qdisc(
            "tc on compute-01 failed: Error: Exclusivity flag on, cannot modify"
        ));
        assert!(!is_absent_qdisc(
            "tc on compute-01 failed: command not found"
        ));
        assert!(!is_absent_qdisc(
            "docker compose exec compute-01 tc failed: No such container"
        ));
    }
}

#[cfg(test)]
mod compose_plan_tests {
    use super::{plan_compose, ClusterTopology};

    #[test]
    fn every_node_is_granted_cap_net_admin() {
        // `tc qdisc` needs CAP_NET_ADMIN in the container's network namespace,
        // and Docker's default capability set does not include it. Without this
        // every shaping call fails with EPERM — the cluster starts, the tests
        // run, and nothing is ever shaped. That is a silent no-op at the level
        // of the whole lane, so it is asserted here, where it costs nothing.
        let (yaml, _) = plan_compose(&ClusterTopology {
            compute_count: 2,
            gpu_count: 1,
            login_count: 1,
            controller: true,
        });
        assert!(
            yaml.contains("cap_add:\n    - NET_ADMIN"),
            "NET_ADMIN must be granted in the ssh-node anchor, got:\n{yaml}"
        );
        // It is granted by the anchor every service merges, so a service cannot
        // exist that lacks it.
        let service_count = yaml.matches("<<: *ssh-node").count();
        assert_eq!(service_count, 5, "expected 5 services, got {service_count}");
    }

    #[test]
    fn every_service_merges_the_anchor() {
        // Each service is `<<: *ssh-node`, so the anchor's build/volume/network/
        // cap_add apply to all of them. A service that stopped merging it would
        // silently lose NET_ADMIN.
        let (yaml, ports) = plan_compose(&ClusterTopology {
            compute_count: 3,
            gpu_count: 0,
            login_count: 0,
            controller: false,
        });
        assert_eq!(ports.len(), 3);
        assert_eq!(yaml.matches("<<: *ssh-node").count(), 3);
        assert!(yaml.contains("cap_add:"));
    }

    #[test]
    fn ports_are_unique_and_follow_the_node_order() {
        let (yaml, ports) = plan_compose(&ClusterTopology {
            compute_count: 2,
            gpu_count: 1,
            login_count: 1,
            controller: true,
        });
        assert_eq!(ports.len(), 5);
        assert_eq!(ports["compute-01"], 2201);
        assert_eq!(ports["compute-02"], 2202);
        assert_eq!(ports["gpu-01"], 2203);
        assert_eq!(ports["login-01"], 2204);
        assert_eq!(ports["controller"], 2205);

        let mut seen: Vec<u16> = ports.values().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 5, "host ports must not collide: {ports:?}");

        for (name, port) in &ports {
            assert!(
                yaml.contains(&format!("- \"{port}:22\"")),
                "port for {name} is not published in the compose file"
            );
        }
    }
}
