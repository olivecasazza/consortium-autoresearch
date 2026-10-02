//! SSH Worker implementation.
//!
//! Provides SshWorker, which wraps ExecWorker to execute commands on remote
//! nodes via SSH. Mirrors ClusterShell's WorkerSsh / SshClient.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::Duration;

use crate::worker::exec::ExecWorker;
use crate::worker::{EventHandler, Worker, WorkerState};

/// SSH connection options.
#[derive(Debug, Clone)]
pub struct SshOptions {
    /// Path to the SSH binary.
    pub ssh_path: String,
    /// Path to the SCP binary.
    pub scp_path: String,
    /// SSH user (if different from current user).
    pub user: Option<String>,
    /// Custom SSH options (passed as -o key=value).
    pub options: Vec<String>,
    /// Connect timeout in seconds.
    pub connect_timeout: Option<u32>,
    /// Whether to use strict host key checking.
    pub strict_host_key_checking: bool,
    /// Whether to enable password authentication.
    pub password_auth: bool,
    /// Path to SSH identity file (passed as -i).
    pub identity_file: Option<String>,
    /// Path to SSH config file (passed as -F).
    pub config_file: Option<String>,
    /// Enable SSH ControlMaster multiplexing.
    pub control_master: Option<ControlMaster>,
    /// Path for ControlMaster socket (passed as -oControlPath=...).
    pub control_path: Option<String>,
    /// ControlPersist timeout (e.g. "10m", "yes").
    pub control_persist: Option<String>,
    /// SSH ProxyJump host (passed as -J).
    pub proxy_jump: Option<String>,
    /// SSH port (passed as -p).
    pub port: Option<u16>,
}

/// SSH ControlMaster modes.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMaster {
    /// Automatically start a master if one isn't running.
    Auto,
    /// Automatically start and automatically close.
    AutoAsk,
    /// Always be the master.
    Yes,
    /// Never be the master.
    No,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            ssh_path: "ssh".to_string(),
            scp_path: "scp".to_string(),
            user: None,
            options: Vec::new(),
            connect_timeout: Some(10),
            strict_host_key_checking: false,
            password_auth: false,
            identity_file: None,
            config_file: None,
            control_master: None,
            control_path: None,
            control_persist: None,
            proxy_jump: None,
            port: None,
        }
    }
}

impl SshOptions {
    /// Build the SSH command prefix for a given node.
    ///
    /// Returns the full SSH command string that will execute `command` on `node`.
    pub fn build_ssh_cmd(&self, node: &str, command: &str) -> String {
        let mut parts = Vec::new();
        parts.push(self.ssh_path.clone());

        // Config file must come early (before other options)
        if let Some(ref config_file) = self.config_file {
            parts.push("-F".to_string());
            parts.push(config_file.clone());
        }

        // Standard hardened options (mirrors Python SshClient._build_cmd)
        parts.push("-oForwardX11=no".to_string());

        if !self.strict_host_key_checking {
            parts.push("-oStrictHostKeyChecking=no".to_string());
        }

        if !self.password_auth {
            parts.push("-oPasswordAuthentication=no".to_string());
        }

        if let Some(timeout) = self.connect_timeout {
            parts.push(format!("-oConnectTimeout={}", timeout));
        }

        // ControlMaster multiplexing
        if let Some(ref cm) = self.control_master {
            let val = match cm {
                ControlMaster::Auto => "auto",
                ControlMaster::AutoAsk => "autoask",
                ControlMaster::Yes => "yes",
                ControlMaster::No => "no",
            };
            parts.push(format!("-oControlMaster={}", val));
        }
        if let Some(ref cp) = self.control_path {
            parts.push(format!("-oControlPath={}", cp));
        }
        if let Some(ref persist) = self.control_persist {
            parts.push(format!("-oControlPersist={}", persist));
        }

        // Custom options
        for opt in &self.options {
            parts.push(format!("-o{}", opt));
        }

        // Identity file
        if let Some(ref identity) = self.identity_file {
            parts.push("-i".to_string());
            parts.push(identity.clone());
        }

        // ProxyJump
        if let Some(ref jump) = self.proxy_jump {
            parts.push("-J".to_string());
            parts.push(jump.clone());
        }

        // Port
        if let Some(port) = self.port {
            parts.push("-p".to_string());
            parts.push(port.to_string());
        }

        // User
        if let Some(ref user) = self.user {
            parts.push("-l".to_string());
            parts.push(user.clone());
        }

        // Target node and command
        parts.push(node.to_string());
        parts.push(command.to_string());

        parts.join(" ")
    }

    /// Build an SCP command for copying files to/from a node.
    ///
    /// If `reverse` is true, copies from remote to local.
    pub fn build_scp_cmd(
        &self,
        node: &str,
        source: &str,
        dest: &str,
        reverse: bool,
        preserve: bool,
        directory: bool,
    ) -> String {
        let mut parts = Vec::new();
        parts.push(self.scp_path.clone());

        // Config file
        if let Some(ref config_file) = self.config_file {
            parts.push("-F".to_string());
            parts.push(config_file.clone());
        }

        if !self.strict_host_key_checking {
            parts.push("-oStrictHostKeyChecking=no".to_string());
        }

        if !self.password_auth {
            parts.push("-oPasswordAuthentication=no".to_string());
        }

        if let Some(timeout) = self.connect_timeout {
            parts.push(format!("-oConnectTimeout={}", timeout));
        }

        // ControlMaster multiplexing
        if let Some(ref cm) = self.control_master {
            let val = match cm {
                ControlMaster::Auto => "auto",
                ControlMaster::AutoAsk => "autoask",
                ControlMaster::Yes => "yes",
                ControlMaster::No => "no",
            };
            parts.push(format!("-oControlMaster={}", val));
        }
        if let Some(ref cp) = self.control_path {
            parts.push(format!("-oControlPath={}", cp));
        }
        if let Some(ref persist) = self.control_persist {
            parts.push(format!("-oControlPersist={}", persist));
        }

        if preserve {
            parts.push("-p".to_string());
        }

        if directory {
            parts.push("-r".to_string());
        }

        // Identity file
        if let Some(ref identity) = self.identity_file {
            parts.push("-i".to_string());
            parts.push(identity.clone());
        }

        // ProxyJump
        if let Some(ref jump) = self.proxy_jump {
            parts.push("-J".to_string());
            parts.push(jump.clone());
        }

        // Port
        if let Some(port) = self.port {
            parts.push("-P".to_string()); // SCP uses -P (uppercase) for port
            parts.push(port.to_string());
        }

        // Custom options
        for opt in &self.options {
            parts.push(format!("-o{}", opt));
        }

        // User prefix
        let user_prefix = if let Some(ref user) = self.user {
            format!("{}@", user)
        } else {
            String::new()
        };

        if reverse {
            // Remote -> Local
            parts.push(format!("{}{}:{}", user_prefix, node, source));
            parts.push(dest.to_string());
        } else {
            // Local -> Remote
            parts.push(source.to_string());
            parts.push(format!("{}{}:{}", user_prefix, node, dest));
        }

        parts.join(" ")
    }
}

/// Worker that executes commands on remote nodes via SSH.
///
/// SshWorker is a thin wrapper around ExecWorker that builds SSH
/// commands using configurable SshOptions. It mirrors ClusterShell's
/// WorkerSsh class.
pub struct SshWorker {
    /// Underlying ExecWorker that does the actual process management.
    inner: ExecWorker,
    /// SSH options used for command building.
    ssh_options: SshOptions,
    /// Original command (before SSH wrapping).
    original_command: String,
}

impl SshWorker {
    /// Create a new SshWorker.
    ///
    /// # Arguments
    /// * `nodes` - List of remote node names to execute on.
    /// * `command` - Command to run on each remote node.
    /// * `fanout` - Max concurrent SSH connections.
    /// * `timeout` - Optional per-node timeout.
    /// * `ssh_options` - SSH connection options.
    pub fn new(
        nodes: Vec<String>,
        command: String,
        fanout: usize,
        timeout: Option<Duration>,
        ssh_options: SshOptions,
    ) -> Self {
        // Build the SSH-wrapped command using %h for node substitution.
        // ExecWorker will replace %h with the actual node name.
        let ssh_cmd = ssh_options.build_ssh_cmd("%h", &command);

        SshWorker {
            inner: ExecWorker::new(nodes, ssh_cmd, fanout, timeout),
            ssh_options,
            original_command: command,
        }
    }

    /// Create a new SshWorker with default SSH options.
    pub fn with_defaults(
        nodes: Vec<String>,
        command: String,
        fanout: usize,
        timeout: Option<Duration>,
    ) -> Self {
        Self::new(nodes, command, fanout, timeout, SshOptions::default())
    }

    /// Enable stderr capture.
    pub fn with_stderr(mut self, stderr: bool) -> Self {
        self.inner = self.inner.with_stderr(stderr);
        self
    }

    /// Get the SSH options.
    pub fn ssh_options(&self) -> &SshOptions {
        &self.ssh_options
    }

    /// Get the original (unwrapped) command.
    pub fn original_command(&self) -> &str {
        &self.original_command
    }
}

impl Worker for SshWorker {
    fn start(&mut self) -> crate::worker::Result<()> {
        self.inner.start()
    }

    fn abort(&mut self, kill: bool) {
        self.inner.abort(kill);
    }

    fn state(&self) -> WorkerState {
        self.inner.state()
    }

    fn set_handler(&mut self, handler: Box<dyn EventHandler>) {
        self.inner.set_handler(handler);
    }

    fn read_fds(&self) -> Vec<RawFd> {
        self.inner.read_fds()
    }

    fn write_fds(&self) -> Vec<RawFd> {
        self.inner.write_fds()
    }

    fn handle_read(&mut self, fd: RawFd) -> crate::worker::Result<()> {
        self.inner.handle_read(fd)
    }

    fn handle_write(&mut self, fd: RawFd) -> crate::worker::Result<()> {
        self.inner.handle_write(fd)
    }

    fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    fn retcodes(&self) -> &HashMap<String, i32> {
        self.inner.retcodes()
    }

    fn num_nodes(&self) -> usize {
        self.inner.num_nodes()
    }

    fn take_handler(&mut self) -> Option<Box<dyn EventHandler>> {
        self.inner.take_handler()
    }
}

/// Worker that copies files to/from remote nodes via SCP.
///
/// Mirrors ClusterShell's ScpClient / WorkerSsh copy mode.
pub struct ScpWorker {
    /// Underlying ExecWorker for running SCP commands.
    inner: ExecWorker,
    /// SSH/SCP options.
    #[allow(dead_code)]
    ssh_options: SshOptions,
    /// Source path.
    source: String,
    /// Destination path.
    dest: String,
    /// Whether this is a reverse copy (remote -> local).
    reverse: bool,
}

/// Direction-specific knobs for an [`ScpWorker`].
///
/// These travel together because they only mean anything together: `reverse`
/// decides which side is local, and `preserve`/`directory` are the attribute
/// and recursion flags for the same copy. Grouping them also keeps the
/// `directory` / `preserve` pair from being swapped at a call site.
#[derive(Debug, Clone, Default)]
pub struct ScpTransfer {
    /// If true, copy from remote to local.
    pub reverse: bool,
    /// If true, preserve file attributes.
    pub preserve: bool,
    /// If true, copy recursively.
    pub directory: bool,
}

impl ScpTransfer {
    /// A plain forward file copy with no attribute preservation.
    pub fn forward() -> Self {
        Self::default()
    }

    /// A forward recursive copy that preserves file attributes.
    pub fn recursive() -> Self {
        Self {
            preserve: true,
            directory: true,
            ..Self::default()
        }
    }

    /// Mark this copy as remote -> local.
    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

impl ScpWorker {
    /// Create a new ScpWorker for file copy.
    ///
    /// # Arguments
    /// * `nodes` - Remote nodes to copy to/from.
    /// * `source` - Source file/directory path.
    /// * `dest` - Destination file/directory path.
    /// * `fanout` - Max concurrent SCP operations.
    /// * `timeout` - Optional per-node timeout.
    /// * `ssh_options` - SSH/SCP options.
    /// * `transfer` - Direction, attribute-preservation and recursion flags.
    pub fn new(
        nodes: Vec<String>,
        source: String,
        dest: String,
        fanout: usize,
        timeout: Option<Duration>,
        ssh_options: SshOptions,
        transfer: ScpTransfer,
    ) -> Self {
        let ScpTransfer {
            reverse,
            preserve,
            directory,
        } = transfer;
        // Build SCP command with %h placeholder for node substitution
        let scp_cmd = ssh_options.build_scp_cmd("%h", &source, &dest, reverse, preserve, directory);

        ScpWorker {
            inner: ExecWorker::new(nodes, scp_cmd, fanout, timeout),
            ssh_options,
            source,
            dest,
            reverse,
        }
    }

    /// Get the source path.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the destination path.
    pub fn dest(&self) -> &str {
        &self.dest
    }

    /// Whether this is a reverse copy.
    pub fn is_reverse(&self) -> bool {
        self.reverse
    }
}

impl Worker for ScpWorker {
    fn start(&mut self) -> crate::worker::Result<()> {
        self.inner.start()
    }

    fn abort(&mut self, kill: bool) {
        self.inner.abort(kill);
    }

    fn state(&self) -> WorkerState {
        self.inner.state()
    }

    fn set_handler(&mut self, handler: Box<dyn EventHandler>) {
        self.inner.set_handler(handler);
    }

    fn read_fds(&self) -> Vec<RawFd> {
        self.inner.read_fds()
    }

    fn write_fds(&self) -> Vec<RawFd> {
        self.inner.write_fds()
    }

    fn handle_read(&mut self, fd: RawFd) -> crate::worker::Result<()> {
        self.inner.handle_read(fd)
    }

    fn handle_write(&mut self, fd: RawFd) -> crate::worker::Result<()> {
        self.inner.handle_write(fd)
    }

    fn is_done(&self) -> bool {
        self.inner.is_done()
    }

    fn retcodes(&self) -> &HashMap<String, i32> {
        self.inner.retcodes()
    }

    fn num_nodes(&self) -> usize {
        self.inner.num_nodes()
    }

    fn take_handler(&mut self) -> Option<Box<dyn EventHandler>> {
        self.inner.take_handler()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_options_default() {
        let opts = SshOptions::default();
        assert_eq!(opts.ssh_path, "ssh");
        assert_eq!(opts.scp_path, "scp");
        assert!(opts.user.is_none());
        assert_eq!(opts.connect_timeout, Some(10));
        assert!(!opts.strict_host_key_checking);
        assert!(!opts.password_auth);
    }

    #[test]
    fn test_ssh_options_build_ssh_cmd() {
        let opts = SshOptions::default();
        let cmd = opts.build_ssh_cmd("node1", "uname -a");

        assert!(cmd.starts_with("ssh "));
        assert!(cmd.contains("-oForwardX11=no"));
        assert!(cmd.contains("-oStrictHostKeyChecking=no"));
        assert!(cmd.contains("-oPasswordAuthentication=no"));
        assert!(cmd.contains("-oConnectTimeout=10"));
        assert!(cmd.contains("node1"));
        assert!(cmd.ends_with("uname -a"));
    }

    #[test]
    fn test_ssh_options_build_ssh_cmd_with_user() {
        let opts = SshOptions {
            user: Some("admin".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "hostname");

        assert!(cmd.contains("-l admin"));
        assert!(cmd.contains("node1"));
    }

    #[test]
    fn test_ssh_options_build_ssh_cmd_custom_options() {
        let opts = SshOptions {
            options: vec!["BatchMode=yes".to_string()],
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "ls");

        assert!(cmd.contains("-oBatchMode=yes"));
    }

    #[test]
    fn test_ssh_options_build_scp_cmd_forward() {
        let opts = SshOptions::default();
        let cmd = opts.build_scp_cmd("node1", "/tmp/file.txt", "/home/user/", false, true, false);

        assert!(cmd.starts_with("scp "));
        assert!(cmd.contains("-p")); // preserve
        assert!(cmd.contains("/tmp/file.txt"));
        assert!(cmd.contains("node1:/home/user/"));
    }

    #[test]
    fn test_ssh_options_build_scp_cmd_reverse() {
        let opts = SshOptions::default();
        let cmd = opts.build_scp_cmd(
            "node1",
            "/remote/file.txt",
            "/local/dir/",
            true,
            false,
            false,
        );

        assert!(cmd.contains("node1:/remote/file.txt"));
        assert!(cmd.ends_with("/local/dir/"));
    }

    #[test]
    fn test_ssh_options_build_scp_cmd_recursive() {
        let opts = SshOptions::default();
        let cmd = opts.build_scp_cmd("node1", "/src/", "/dst/", false, false, true);

        assert!(cmd.contains("-r")); // recursive
    }

    #[test]
    fn test_ssh_options_build_scp_cmd_with_user() {
        let opts = SshOptions {
            user: Some("deploy".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_scp_cmd("node1", "/f", "/d", false, false, false);

        assert!(cmd.contains("deploy@node1:/d"));
    }

    #[test]
    fn test_ssh_worker_creation() {
        let nodes = vec!["node1".to_string(), "node2".to_string()];
        let worker = SshWorker::with_defaults(nodes, "hostname".to_string(), 64, None);

        assert_eq!(worker.state(), WorkerState::Pending);
        assert_eq!(worker.num_nodes(), 2);
        assert_eq!(worker.original_command(), "hostname");
    }

    #[test]
    fn test_ssh_worker_custom_options() {
        let opts = SshOptions {
            user: Some("root".to_string()),
            connect_timeout: Some(5),
            ..SshOptions::default()
        };
        let nodes = vec!["node1".to_string()];
        let worker = SshWorker::new(nodes, "uptime".to_string(), 64, None, opts);

        assert_eq!(worker.ssh_options().user.as_deref(), Some("root"));
        assert_eq!(worker.ssh_options().connect_timeout, Some(5));
    }

    #[test]
    fn test_ssh_worker_with_stderr() {
        let nodes = vec!["node1".to_string()];
        let worker = SshWorker::with_defaults(nodes, "ls".to_string(), 64, None).with_stderr(true);

        assert_eq!(worker.state(), WorkerState::Pending);
    }

    #[test]
    fn test_scp_worker_creation() {
        let nodes = vec!["node1".to_string()];
        let opts = SshOptions::default();
        let worker = ScpWorker::new(
            nodes,
            "/local/file".to_string(),
            "/remote/dir".to_string(),
            64,
            None,
            opts,
            ScpTransfer {
                preserve: true,
                ..ScpTransfer::forward()
            },
        );

        assert_eq!(worker.state(), WorkerState::Pending);
        assert_eq!(worker.source(), "/local/file");
        assert_eq!(worker.dest(), "/remote/dir");
        assert!(!worker.is_reverse());
    }

    #[test]
    fn test_scp_worker_reverse() {
        let nodes = vec!["node1".to_string()];
        let opts = SshOptions::default();
        let worker = ScpWorker::new(
            nodes,
            "/remote/file".to_string(),
            "/local/dir".to_string(),
            64,
            None,
            opts,
            ScpTransfer::forward().reverse(),
        );

        assert!(worker.is_reverse());
        assert_eq!(worker.num_nodes(), 1);
    }

    #[test]
    fn test_ssh_options_with_identity_file() {
        let opts = SshOptions {
            identity_file: Some("/path/to/key".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "hostname");
        assert!(cmd.contains("-i /path/to/key"));
    }

    #[test]
    fn test_ssh_options_with_config_file() {
        let opts = SshOptions {
            config_file: Some("/etc/ssh/config".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "hostname");
        assert!(cmd.contains("-F /etc/ssh/config"));
    }

    #[test]
    fn test_ssh_options_with_control_master() {
        let opts = SshOptions {
            control_master: Some(ControlMaster::Auto),
            control_path: Some("/tmp/ssh-%r@%h:%p".to_string()),
            control_persist: Some("10m".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "hostname");
        assert!(cmd.contains("-oControlMaster=auto"));
        assert!(cmd.contains("-oControlPath=/tmp/ssh-%r@%h:%p"));
        assert!(cmd.contains("-oControlPersist=10m"));
    }

    #[test]
    fn test_ssh_options_with_proxy_jump() {
        let opts = SshOptions {
            proxy_jump: Some("bastion.example.com".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_ssh_cmd("node1", "hostname");
        assert!(cmd.contains("-J bastion.example.com"));
    }

    #[test]
    fn test_ssh_options_with_port() {
        let opts = SshOptions {
            port: Some(2222),
            ..SshOptions::default()
        };
        let ssh_cmd = opts.build_ssh_cmd("node1", "hostname");
        assert!(ssh_cmd.contains("-p 2222"));

        let scp_cmd = opts.build_scp_cmd("node1", "/src", "/dst", false, false, false);
        assert!(scp_cmd.contains("-P 2222")); // SCP uses uppercase -P
    }

    #[test]
    fn test_scp_options_with_control_master() {
        let opts = SshOptions {
            control_master: Some(ControlMaster::Auto),
            control_path: Some("/tmp/ssh-%r@%h:%p".to_string()),
            identity_file: Some("/key".to_string()),
            ..SshOptions::default()
        };
        let cmd = opts.build_scp_cmd("node1", "/src", "/dst", false, false, false);
        assert!(cmd.contains("-oControlMaster=auto"));
        assert!(cmd.contains("-oControlPath=/tmp/ssh-%r@%h:%p"));
        assert!(cmd.contains("-i /key"));
    }

    #[test]
    fn test_ssh_worker_abort() {
        let nodes = vec!["node1".to_string()];
        let mut worker = SshWorker::with_defaults(nodes, "sleep 100".to_string(), 64, None);

        worker.abort(true);
        assert_eq!(worker.state(), WorkerState::Aborted);
    }
}
