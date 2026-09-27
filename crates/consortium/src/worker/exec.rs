//! ExecWorker implementation for running commands on nodes.
//!
//! This module provides ExecWorker, a worker implementation that
//! spawns child processes to execute commands on remote or local nodes.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Set a file descriptor to non-blocking mode.
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

use crate::worker::{EventHandler, Result, Worker, WorkerError, WorkerState};

/// A child process being managed by ExecWorker.
struct ChildProcess {
    /// The underlying child process.
    child: Child,
    /// File descriptor for stdout.
    stdout_fd: RawFd,
    /// File descriptor for stderr, if captured.
    stderr_fd: Option<RawFd>,
    /// Node name this process is running on.
    node: String,
    /// Buffered stdout data.
    buf_stdout: Vec<u8>,
    /// Buffered stderr data.
    buf_stderr: Vec<u8>,
    /// Whether stderr is being read.
    read_stderr: bool,
}

impl ChildProcess {
    /// Create a new ChildProcess wrapper.
    fn new(child: Child, node: String, read_stderr: bool) -> std::io::Result<Self> {
        let stdout_fd = child.stdout.as_ref().map_or(-1, |s| s.as_raw_fd());
        let stderr_fd = if read_stderr {
            child.stderr.as_ref().map(|s| s.as_raw_fd())
        } else {
            None
        };

        // Set fds to non-blocking so polling reads don't hang
        if stdout_fd >= 0 {
            let _ = set_nonblocking(stdout_fd);
        }
        if let Some(fd) = stderr_fd {
            let _ = set_nonblocking(fd);
        }

        Ok(ChildProcess {
            child,
            stdout_fd,
            stderr_fd,
            node,
            buf_stdout: Vec::new(),
            buf_stderr: Vec::new(),
            read_stderr,
        })
    }

    /// Get file descriptors for read interest.
    #[allow(dead_code)]
    fn read_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.stdout_fd];
        if self.read_stderr {
            if let Some(fd) = self.stderr_fd {
                fds.push(fd);
            }
        }
        fds
    }

    /// Check if this process is done (exited).
    #[allow(dead_code)]
    fn is_done(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Get the exit status if available.
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.child
            .try_wait()
            .map(|opt| opt.map(|status| status.code().unwrap_or(-1)))
    }
}

/// Worker implementation that runs commands on nodes via local execution.
///
/// ExecWorker spawns child processes to execute commands on nodes.
/// It supports fanout control (limiting concurrent processes) and
/// node name substitution in commands (%h or {node}).
pub struct ExecWorker {
    /// List of nodes to run on.
    nodes: Vec<String>,
    /// Command to execute (may contain %h or {node} placeholders).
    command: String,
    /// Current worker state.
    state: WorkerState,
    /// Event handler for worker events.
    handler: Option<Box<dyn EventHandler>>,
    /// Map of node name to child process.
    processes: HashMap<String, ChildProcess>,
    /// Map of node name to return code.
    retcodes: HashMap<String, i32>,
    /// Maximum number of concurrent processes.
    fanout: usize,
    /// Optional timeout for each node's command.
    #[allow(dead_code)]
    timeout: Option<Duration>,
    /// Whether to capture stderr.
    stderr: bool,
    /// Queue of nodes waiting to be processed.
    pending_queue: Vec<String>,
    /// Number of currently running processes.
    running_count: usize,
}

impl ExecWorker {
    /// Create a new ExecWorker.
    ///
    /// # Arguments
    /// * `nodes` - List of node names to execute the command on.
    /// * `command` - Command to execute. Use %h or {node} as placeholders for the node name.
    /// * `fanout` - Maximum number of concurrent processes (default: 64).
    /// * `timeout` - Optional timeout per node.
    pub fn new(
        nodes: Vec<String>,
        command: String,
        fanout: usize,
        timeout: Option<Duration>,
    ) -> Self {
        ExecWorker {
            nodes: nodes.clone(),
            command,
            state: WorkerState::Pending,
            handler: None,
            processes: HashMap::new(),
            retcodes: HashMap::new(),
            fanout,
            timeout,
            stderr: false,
            pending_queue: nodes,
            running_count: 0,
        }
    }

    /// Create a new ExecWorker with stderr enabled.
    pub fn with_stderr(mut self, stderr: bool) -> Self {
        self.stderr = stderr;
        self
    }

    /// Substitute node placeholders in the command.
    fn substitute_node(&self, command: &str, node: &str) -> String {
        command.replace("%h", node).replace("{node}", node)
    }

    /// Spawn a new child process for a node.
    fn spawn_node(&mut self, node: &str) -> Result<()> {
        let cmd = self.substitute_node(&self.command, node);
        let child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(Stdio::piped())
            .stderr(if self.stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .spawn()
            .map_err(|e| {
                WorkerError::General(format!("failed to spawn process for {}: {}", node, e))
            })?;

        let process =
            ChildProcess::new(child, node.to_string(), self.stderr).map_err(WorkerError::Io)?;

        let node_name = process.node.clone();
        self.processes.insert(node_name.clone(), process);
        self.running_count += 1;

        // Temporarily take handler to avoid borrow conflict
        if let Some(mut handler) = self.handler.take() {
            handler.on_start(self);
            self.handler = Some(handler);
        }

        Ok(())
    }

    /// Read available data from a file descriptor and notify handler.
    fn read_from_fd(&mut self, fd: RawFd) -> Result<bool> {
        let mut read_something = false;
        // Collect handler events to dispatch after iteration ends,
        // since we can't borrow self.handler while iterating self.processes.
        let mut events: Vec<(String, RawFd, Vec<u8>)> = Vec::new();

        // Try to find which process this fd belongs to
        for (node, process) in self.processes.iter_mut() {
            let is_stdout = process.stdout_fd == fd;
            let is_stderr = process.read_stderr && process.stderr_fd == Some(fd);

            if !is_stdout && !is_stderr {
                continue;
            }

            let mut buffer = [0u8; 4096];

            // Read from the child's stdout/stderr handles directly
            let read_result = if is_stdout {
                if let Some(ref mut stdout) = process.child.stdout {
                    stdout.read(&mut buffer)
                } else {
                    continue;
                }
            } else if let Some(ref mut stderr) = process.child.stderr {
                stderr.read(&mut buffer)
            } else {
                continue;
            };

            match read_result {
                Ok(0) => {
                    // EOF
                    continue;
                }
                Ok(n) => {
                    let msg = &buffer[..n];
                    read_something = true;

                    let event_fd = if is_stdout { process.stdout_fd } else { fd };

                    // Store in buffer
                    if is_stdout {
                        process.buf_stdout.extend_from_slice(msg);
                    } else {
                        process.buf_stderr.extend_from_slice(msg);
                    }

                    // Defer handler notification
                    events.push((node.clone(), event_fd, msg.to_vec()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    return Err(WorkerError::Io(e));
                }
            }
        }

        // Dispatch deferred handler notifications
        if let Some(ref mut handler) = self.handler {
            for (node, event_fd, msg) in &events {
                handler.on_read(node, *event_fd, msg);
            }
        }

        Ok(read_something)
    }

    /// Drain all remaining data from a process's stdout/stderr.
    /// Must be called before removing the process so no output is lost.
    fn drain_process(process: &mut ChildProcess, handler: &mut Option<Box<dyn EventHandler>>) {
        let mut buffer = [0u8; 4096];

        // Drain stdout
        if let Some(ref mut stdout) = process.child.stdout {
            loop {
                match stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let msg = &buffer[..n];
                        process.buf_stdout.extend_from_slice(msg);
                        if let Some(ref mut h) = handler {
                            h.on_read(&process.node, process.stdout_fd, msg);
                        }
                    }
                }
            }
        }

        // Drain stderr
        if process.read_stderr {
            if let Some(ref mut stderr) = process.child.stderr {
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let msg = &buffer[..n];
                            process.buf_stderr.extend_from_slice(msg);
                            if let Some(ref mut h) = handler {
                                if let Some(fd) = process.stderr_fd {
                                    h.on_read(&process.node, fd, msg);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check for exited processes and update state.
    fn check_exited_processes(&mut self) -> Result<bool> {
        let mut something_changed = false;
        let mut exited_nodes: Vec<String> = Vec::new();

        for (node, process) in self.processes.iter_mut() {
            if let Ok(Some(rc)) = process.try_wait() {
                exited_nodes.push(node.clone());
                self.retcodes.insert(node.clone(), rc);
            }
        }

        for node in &exited_nodes {
            // Drain remaining output before removing the process
            if let Some(process) = self.processes.get_mut(node) {
                Self::drain_process(process, &mut self.handler);
            }

            // Notify handler of close
            if let Some(ref mut handler) = self.handler {
                let rc = self.retcodes.get(node).copied().unwrap_or(-1);
                handler.on_close(node, rc);
            }
        }

        for node in exited_nodes {
            self.processes.remove(&node);
            self.running_count -= 1;
            something_changed = true;
        }

        Ok(something_changed)
    }
}

impl Worker for ExecWorker {
    fn start(&mut self) -> Result<()> {
        if self.state != WorkerState::Pending {
            return Err(WorkerError::General(
                "worker already started or aborted".to_string(),
            ));
        }

        self.state = WorkerState::Running;

        // Spawn initial batch up to fanout
        while self.running_count < self.fanout && !self.pending_queue.is_empty() {
            let node = self.pending_queue.remove(0);
            self.spawn_node(&node)?;
        }

        Ok(())
    }

    fn abort(&mut self, kill: bool) {
        if self.state == WorkerState::Done || self.state == WorkerState::Aborted {
            return;
        }

        if kill {
            // Kill all running processes
            for (_node, process) in self.processes.iter_mut() {
                let _ = process.child.kill();
            }
        }

        // Clean up processes
        self.processes.clear();
        self.pending_queue.clear();
        self.running_count = 0;
        self.state = WorkerState::Aborted;
    }

    fn state(&self) -> WorkerState {
        self.state
    }

    fn set_handler(&mut self, handler: Box<dyn EventHandler>) {
        self.handler = Some(handler);
    }

    fn read_fds(&self) -> Vec<RawFd> {
        let mut fds = Vec::new();
        for process in self.processes.values() {
            fds.push(process.stdout_fd);
            if process.read_stderr {
                if let Some(fd) = process.stderr_fd {
                    fds.push(fd);
                }
            }
        }
        fds
    }

    fn write_fds(&self) -> Vec<RawFd> {
        // ExecWorker doesn't currently write to child processes
        Vec::new()
    }

    fn handle_read(&mut self, fd: RawFd) -> Result<()> {
        // Read data from the fd
        self.read_from_fd(fd)?;

        // Check for exited processes
        self.check_exited_processes()?;

        // Spawn next pending node if space available
        while self.running_count < self.fanout && !self.pending_queue.is_empty() {
            let node = self.pending_queue.remove(0);
            self.spawn_node(&node)?;
        }

        // Check if we're done
        if self.processes.is_empty() && self.pending_queue.is_empty() {
            self.state = WorkerState::Done;
        }

        Ok(())
    }

    fn handle_write(&mut self, _fd: RawFd) -> Result<()> {
        // No-op for ExecWorker
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.state == WorkerState::Done || self.state == WorkerState::Aborted
    }

    fn retcodes(&self) -> &HashMap<String, i32> {
        &self.retcodes
    }

    fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    fn take_handler(&mut self) -> Option<Box<dyn EventHandler>> {
        self.handler.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Test ExecWorker creation
    #[test]
    fn test_exec_worker_creation() {
        let nodes = vec!["localhost".to_string()];
        let worker = ExecWorker::new(nodes, "echo hello".to_string(), 1, None);

        assert_eq!(worker.state(), WorkerState::Pending);
        assert_eq!(worker.num_nodes(), 1);
        assert!(worker.retcodes().is_empty());
    }

    /// Test ExecWorker with multiple nodes
    #[test]
    fn test_exec_worker_multiple_nodes() {
        let nodes = vec![
            "node1".to_string(),
            "node2".to_string(),
            "node3".to_string(),
        ];
        let worker = ExecWorker::new(nodes, "echo test".to_string(), 2, None);

        assert_eq!(worker.state(), WorkerState::Pending);
        assert_eq!(worker.num_nodes(), 3);
    }

    /// Test ExecWorker with timeout
    #[test]
    fn test_exec_worker_with_timeout() {
        let nodes = vec!["localhost".to_string()];
        let timeout = Duration::from_secs(30);
        let worker = ExecWorker::new(nodes, "sleep 1".to_string(), 1, Some(timeout));

        assert_eq!(worker.state(), WorkerState::Pending);
    }

    /// Test ExecWorker with stderr enabled
    #[test]
    fn test_exec_worker_with_stderr() {
        let nodes = vec!["localhost".to_string()];
        let worker = ExecWorker::new(nodes, "echo test".to_string(), 1, None).with_stderr(true);

        assert!(worker.stderr);
    }

    /// Test node substitution in command (%h placeholder)
    #[test]
    fn test_node_substitution_percent_h() {
        let nodes = vec!["myhost".to_string()];
        let worker = ExecWorker::new(nodes, "ssh %h 'ls'".to_string(), 1, None);

        let cmd = worker.substitute_node("ssh %h 'ls'", "myhost");
        assert_eq!(cmd, "ssh myhost 'ls'");
    }

    /// Test node substitution in command ({node} placeholder)
    #[test]
    fn test_node_substitution_braces() {
        let nodes = vec!["myhost".to_string()];
        let worker = ExecWorker::new(nodes, "ssh {node} 'ls'".to_string(), 1, None);

        let cmd = worker.substitute_node("ssh {node} 'ls'", "myhost");
        assert_eq!(cmd, "ssh myhost 'ls'");
    }

    /// Test ExecWorker with simple echo command on localhost
    #[test]
    fn test_exec_worker_simple_echo() {
        let nodes = vec!["localhost".to_string()];
        let mut worker = ExecWorker::new(nodes, "echo hello".to_string(), 1, None);

        // Worker should start in Pending state
        assert_eq!(worker.state(), WorkerState::Pending);

        // Start the worker
        let result = worker.start();
        assert!(result.is_ok());

        // Worker should now be Running
        assert_eq!(worker.state(), WorkerState::Running);

        // Poll for completion
        let start = std::time::Instant::now();
        while !worker.is_done() && start.elapsed() < Duration::from_secs(5) {
            let fds = worker.read_fds();
            for fd in &fds {
                let _ = worker.handle_read(*fd);
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Worker should be Done
        assert!(worker.is_done() || worker.state() == WorkerState::Done);

        // Should have a return code
        let retcodes = worker.retcodes();
        assert!(!retcodes.is_empty());
        assert!(retcodes.contains_key("localhost"));
    }

    /// Test EventHandler integration
    #[test]
    fn test_exec_worker_with_handler() {
        let nodes = vec!["localhost".to_string()];
        let mut worker = ExecWorker::new(nodes, "echo test".to_string(), 1, None);

        struct TestHandler {
            pub on_start_count: Arc<AtomicUsize>,
            pub on_read_count: Arc<AtomicUsize>,
            pub on_close_count: Arc<AtomicUsize>,
        }

        impl EventHandler for TestHandler {
            fn on_start(&mut self, _worker: &dyn Worker) {
                self.on_start_count.fetch_add(1, Ordering::SeqCst);
            }

            fn on_read(&mut self, _node: &str, _fd: RawFd, _msg: &[u8]) {
                self.on_read_count.fetch_add(1, Ordering::SeqCst);
            }

            fn on_close(&mut self, _node: &str, _rc: i32) {
                self.on_close_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let on_start_count = Arc::new(AtomicUsize::new(0));
        let on_read_count = Arc::new(AtomicUsize::new(0));
        let on_close_count = Arc::new(AtomicUsize::new(0));

        let handler = TestHandler {
            on_start_count: on_start_count.clone(),
            on_read_count: on_read_count.clone(),
            on_close_count: on_close_count.clone(),
        };

        worker.set_handler(Box::new(handler));

        // Start and run
        let _ = worker.start();

        let start = std::time::Instant::now();
        while !worker.is_done() && start.elapsed() < Duration::from_secs(5) {
            let fds = worker.read_fds();
            for fd in &fds {
                let _ = worker.handle_read(*fd);
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // Handlers should have been called
        assert!(
            on_start_count.load(Ordering::SeqCst) >= 1,
            "on_start should be called"
        );
        assert!(
            on_close_count.load(Ordering::SeqCst) >= 1,
            "on_close should be called"
        );
    }

    /// Test fanout control
    #[test]
    fn test_exec_worker_fanout() {
        let nodes = vec![
            "node1".to_string(),
            "node2".to_string(),
            "node3".to_string(),
            "node4".to_string(),
        ];
        let mut worker = ExecWorker::new(nodes, "echo test".to_string(), 2, None);

        // Fanout is 2, so only 2 processes should start initially
        let _ = worker.start();

        // Give a moment for processes to start
        std::thread::sleep(Duration::from_millis(50));

        // Should have at most 2 running processes
        assert!(
            worker.running_count <= 2,
            "Should have at most fanout processes running"
        );
    }

    /// Test abort functionality
    #[test]
    fn test_exec_worker_abort() {
        let nodes = vec!["localhost".to_string()];
        let mut worker = ExecWorker::new(nodes, "sleep 10".to_string(), 1, None);

        let _ = worker.start();
        assert_eq!(worker.state(), WorkerState::Running);

        worker.abort(true);
        assert_eq!(worker.state(), WorkerState::Aborted);
        assert!(worker.processes.is_empty());
    }

    /// Test that read_fds returns correct file descriptors
    #[test]
    fn test_exec_worker_read_fds() {
        let nodes = vec!["localhost".to_string()];
        let worker = ExecWorker::new(nodes, "echo test".to_string(), 1, None);

        let fds = worker.read_fds();
        assert!(fds.is_empty(), "No fds before start");
    }

    /// Test that write_fds returns empty for ExecWorker
    #[test]
    fn test_exec_worker_write_fds() {
        let nodes = vec!["localhost".to_string()];
        let worker = ExecWorker::new(nodes, "echo test".to_string(), 1, None);

        let fds = worker.write_fds();
        assert!(fds.is_empty(), "ExecWorker doesn't write to children");
    }

    /// Test that all nodes produce output even when processes exit quickly.
    /// Regression test: fast-exiting processes must have their output drained
    /// before being reaped, otherwise data is lost.
    #[test]
    fn test_exec_worker_all_nodes_produce_output() {
        use std::collections::HashSet;
        use std::sync::Mutex;

        let nodes: Vec<String> = (1..=5).map(|i| format!("node{i}")).collect();
        let mut worker =
            ExecWorker::new(nodes.clone(), "echo output_from_%h".to_string(), 64, None);

        let collected = Arc::new(Mutex::new(HashMap::<String, Vec<u8>>::new()));
        let collected_clone = collected.clone();
        let close_count = Arc::new(AtomicUsize::new(0));
        let close_count_clone = close_count.clone();

        struct CollectorHandler {
            collected: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            close_count: Arc<AtomicUsize>,
        }

        impl EventHandler for CollectorHandler {
            fn on_start(&mut self, _worker: &dyn Worker) {}

            fn on_read(&mut self, node: &str, _fd: RawFd, msg: &[u8]) {
                let mut map = self.collected.lock().unwrap();
                map.entry(node.to_string())
                    .or_default()
                    .extend_from_slice(msg);
            }

            fn on_close(&mut self, _node: &str, _rc: i32) {
                self.close_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        worker.set_handler(Box::new(CollectorHandler {
            collected: collected_clone,
            close_count: close_count_clone,
        }));

        let _ = worker.start();

        let start = std::time::Instant::now();
        while !worker.is_done() && start.elapsed() < Duration::from_secs(10) {
            let fds = worker.read_fds();
            for fd in &fds {
                let _ = worker.handle_read(*fd);
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(worker.is_done(), "worker should finish");

        // All 5 nodes should have been closed
        assert_eq!(
            close_count.load(Ordering::SeqCst),
            5,
            "all nodes should close"
        );

        // All 5 nodes should have produced output
        let map = collected.lock().unwrap();
        let nodes_with_output: HashSet<&String> = map.keys().collect();
        for node in &nodes {
            assert!(
                nodes_with_output.contains(node),
                "node {} should have output, got nodes: {:?}",
                node,
                nodes_with_output
            );
            let output = String::from_utf8_lossy(&map[node]);
            assert!(
                output.contains(&format!("output_from_{node}")),
                "node {} should have correct output, got: {}",
                node,
                output
            );
        }
    }
}
