//! Task orchestration.
//!
//! Rust implementation of `ClusterShell.Task`.
//!
//! A Task manages a coordinated collection of independent parallel Worker
//! objects, providing a high-level API for executing commands locally or
//! across remote nodes and collecting their results.
//!
//! # Example
//!
//! ```rust,no_run
//! use consortium::task::Task;
//!
//! let mut task = Task::new();
//! task.shell("hostname", None, None);
//! task.run(None).unwrap();
//!
//! for (rc, nodes) in task.iter_retcodes(None) {
//!     println!("rc={}: {:?}", rc, nodes);
//! }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use crate::defaults::{config_paths, Defaults};
use crate::msg_tree::{MsgTree, MsgTreeMode};
use crate::node_set::NodeSet;
use crate::propagation::PropagationTreeRouter;
use crate::topology::TopologyParser;
use crate::worker::exec::ExecWorker;
use crate::worker::{EventHandler, Worker, WorkerBuffers, WorkerError, WorkerState};

/// Error types for the task module.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    /// A worker error occurred.
    #[error("worker error: {0}")]
    Worker(#[from] WorkerError),
    /// Task timed out.
    #[error("task timeout")]
    Timeout,
    /// Task is already running.
    #[error("task is already running")]
    AlreadyRunning,
    /// MsgTree not enabled for the requested stream.
    #[error("{0}_msgtree not set")]
    MsgTreeDisabled(String),
    /// Topology error.
    #[error("topology error: {0}")]
    Topology(String),
    /// General task error.
    #[error("task error: {0}")]
    General(String),
}

/// Result type alias for task operations.
pub type Result<T> = std::result::Result<T, TaskError>;

/// Identifies a source of output: a (worker_id, node_name) pair.
///
/// In the Python version, the source is `(worker_instance, node_string)`.
/// Here we use a numeric worker ID for ownership safety.
pub type Source = (usize, String);

/// Current state of the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Task is idle — workers may be queued but not running.
    Idle,
    /// Task is currently running its workers.
    Running,
    /// Task has completed.
    Done,
    /// Task was aborted.
    Aborted,
}

/// Configuration for the task's default behavior.
///
/// Maps to ClusterShell's `_task_default` dictionary.
#[derive(Debug, Clone)]
pub struct TaskDefaults {
    /// Whether to enable stderr separation (default: false).
    pub stderr: bool,
    /// Whether to enable stdin (default: true).
    pub stdin: bool,
    /// Whether to record stdout into a MsgTree (default: true).
    pub stdout_msgtree: bool,
    /// Whether to record stderr into a MsgTree (default: true).
    pub stderr_msgtree: bool,
    /// Auto tree mode (use tree topology when available).
    pub auto_tree: bool,
}

impl Default for TaskDefaults {
    fn default() -> Self {
        Self {
            stderr: false,
            stdin: true,
            stdout_msgtree: true,
            stderr_msgtree: true,
            auto_tree: false,
        }
    }
}

impl TaskDefaults {
    /// Create task defaults from a Defaults config.
    pub fn from_defaults(defaults: &Defaults) -> Self {
        Self {
            stderr: defaults.stderr(),
            stdin: defaults.stdin(),
            stdout_msgtree: true,
            stderr_msgtree: true,
            auto_tree: false,
        }
    }
}

/// Runtime info for the task.
///
/// Maps to ClusterShell's `_task_info` dictionary.
#[derive(Debug, Clone)]
pub struct TaskInfo {
    /// Enable debug messages.
    pub debug: bool,
    /// Maximum fanout (concurrent workers).
    pub fanout: usize,
    /// Grooming delay for message traffic shaping (seconds).
    pub grooming_delay: f64,
    /// Connection timeout (seconds).
    pub connect_timeout: f64,
    /// Command timeout (seconds, 0 = unlimited).
    pub command_timeout: f64,
}

impl Default for TaskInfo {
    fn default() -> Self {
        Self {
            debug: false,
            fanout: 64,
            grooming_delay: 0.5,
            connect_timeout: 10.0,
            command_timeout: 0.0,
        }
    }
}

impl TaskInfo {
    /// Create task info from a Defaults config.
    pub fn from_defaults(defaults: &Defaults) -> Self {
        Self {
            debug: false,
            fanout: defaults.fanout() as usize,
            grooming_delay: 0.5,
            connect_timeout: defaults.connect_timeout(),
            command_timeout: defaults.command_timeout(),
        }
    }
}

/// Topology configuration file paths.
fn topology_configs() -> Vec<PathBuf> {
    config_paths("topology.conf")
}

/// Get the short hostname (hostname up to the first dot).
fn get_short_hostname() -> String {
    hostname::get()
        .map(|h: std::ffi::OsString| {
            h.to_string_lossy()
                .split('.')
                .next()
                .unwrap_or("localhost")
                .to_string()
        })
        .unwrap_or_else(|_| "localhost".to_string())
}

/// A managed worker entry inside a Task.
struct ManagedWorker {
    /// The worker instance.
    worker: Box<dyn Worker>,
    /// Whether this worker updates task-level return codes.
    update_task_rc: bool,
    /// Whether this worker auto-closes when all other workers finish.
    autoclose: bool,
}

/// An event-gathering handler that records output into the Task's result stores.
///
/// This handler is installed on workers when they are scheduled. It forwards
/// data to the task's internal MsgTrees and retcode maps via channels or
/// direct callback.
struct TaskGatheringHandler {
    /// Worker ID that this handler is associated with.
    #[allow(dead_code)]
    worker_id: usize,
    /// Collected stdout lines: node -> Vec<lines>.
    stdout: HashMap<String, Vec<Vec<u8>>>,
    /// Collected stderr lines: node -> Vec<lines>.
    stderr: HashMap<String, Vec<Vec<u8>>>,
    /// Return codes: node -> rc.
    retcodes: HashMap<String, i32>,
    /// Timed-out nodes.
    timeouts: HashSet<String>,
    /// User-provided handler to chain to, if any.
    user_handler: Option<Box<dyn EventHandler>>,
}

impl TaskGatheringHandler {
    fn new(worker_id: usize, user_handler: Option<Box<dyn EventHandler>>) -> Self {
        Self {
            worker_id,
            stdout: HashMap::new(),
            stderr: HashMap::new(),
            retcodes: HashMap::new(),
            timeouts: HashSet::new(),
            user_handler,
        }
    }
}

impl EventHandler for TaskGatheringHandler {
    fn on_start(&mut self, worker: &dyn Worker) {
        if let Some(ref mut h) = self.user_handler {
            h.on_start(worker);
        }
    }

    fn on_read(&mut self, node: &str, fd: std::os::unix::io::RawFd, msg: &[u8]) {
        // fd 1 = stdout, fd 2 = stderr
        let map = if fd == 2 {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        map.entry(node.to_string()).or_default().push(msg.to_vec());

        if let Some(ref mut h) = self.user_handler {
            h.on_read(node, fd, msg);
        }
    }

    fn on_close(&mut self, node: &str, rc: i32) {
        self.retcodes.insert(node.to_string(), rc);

        if let Some(ref mut h) = self.user_handler {
            h.on_close(node, rc);
        }
    }

    fn on_timeout(&mut self, node: &str) {
        self.timeouts.insert(node.to_string());

        if let Some(ref mut h) = self.user_handler {
            h.on_timeout(node);
        }
    }

    fn on_error(&mut self, node: &str, error: &WorkerError) {
        if let Some(ref mut h) = self.user_handler {
            h.on_error(node, error);
        }
    }

    fn take_buffers(&mut self) -> WorkerBuffers {
        WorkerBuffers {
            stdout: std::mem::take(&mut self.stdout),
            stderr: std::mem::take(&mut self.stderr),
            timeouts: std::mem::take(&mut self.timeouts),
        }
    }
}

/// A Task manages a set of workers and their results.
///
/// The Task is the primary orchestrator in consortium. It schedules workers
/// for execution, runs them (respecting fanout limits), and collects their
/// output, return codes, and timeout information.
///
/// # Design Differences from Python
///
/// The Python Task is heavily thread-based with a thread-local singleton
/// pattern (`task_self()`). The Rust version is single-threaded and owned,
/// using explicit lifetimes rather than thread-local storage. Multi-threaded
/// usage should wrap `Task` in `Arc<Mutex<Task>>` if needed.
pub struct Task {
    /// Task state.
    state: TaskState,
    /// Task-level defaults.
    defaults: TaskDefaults,
    /// Task-level runtime info.
    info: TaskInfo,
    /// Global task timeout (set via resume/run).
    timeout: Option<Duration>,

    // -- Worker management --
    /// Next worker ID to assign.
    next_worker_id: usize,
    /// Managed workers (worker_id -> ManagedWorker).
    workers: Vec<(usize, ManagedWorker)>,

    // -- Result stores --
    /// MsgTree for stdout, keyed by (worker_id, node).
    stdout_tree: Option<MsgTree<Source>>,
    /// MsgTree for stderr, keyed by (worker_id, node).
    stderr_tree: Option<MsgTree<Source>>,
    /// Return codes by source: (worker_id, node) -> rc.
    d_source_rc: HashMap<Source, i32>,
    /// Sources by return code: rc -> set of sources.
    d_rc_sources: BTreeMap<i32, HashSet<Source>>,
    /// Maximum return code seen.
    max_rc: Option<i32>,
    /// Timed-out sources.
    timeout_sources: HashSet<Source>,

    // -- Topology / Tree mode --
    /// Loaded topology parser (owns the tree).
    topology_parser: Option<TopologyParser>,
    /// Propagation tree router (shared across tree workers).
    router: Option<PropagationTreeRouter>,
    /// Gateway channels: gateway_name -> worker_id.
    #[allow(dead_code)]
    gateways: HashMap<String, usize>,
}

impl Task {
    /// Create a new Task with default settings.
    pub fn new() -> Self {
        let defaults = TaskDefaults::default();
        let info = TaskInfo::default();
        Self::with_config(defaults, info)
    }

    /// Create a new Task from a Defaults configuration.
    pub fn from_defaults(defaults: &Defaults) -> Self {
        let task_defaults = TaskDefaults::from_defaults(defaults);
        let task_info = TaskInfo::from_defaults(defaults);
        Self::with_config(task_defaults, task_info)
    }

    /// Create a new Task with explicit configuration.
    pub fn with_config(defaults: TaskDefaults, info: TaskInfo) -> Self {
        let stdout_tree = if defaults.stdout_msgtree {
            Some(MsgTree::new(MsgTreeMode::Shift))
        } else {
            None
        };
        let stderr_tree = if defaults.stderr_msgtree {
            Some(MsgTree::new(MsgTreeMode::Shift))
        } else {
            None
        };

        Self {
            state: TaskState::Idle,
            defaults,
            info,
            timeout: None,
            next_worker_id: 0,
            workers: Vec::new(),
            stdout_tree,
            stderr_tree,
            d_source_rc: HashMap::new(),
            d_rc_sources: BTreeMap::new(),
            max_rc: None,
            timeout_sources: HashSet::new(),
            topology_parser: None,
            router: None,
            gateways: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Defaults & Info
    // -----------------------------------------------------------------------

    /// Get task defaults.
    pub fn defaults(&self) -> &TaskDefaults {
        &self.defaults
    }

    /// Get mutable task defaults.
    pub fn defaults_mut(&mut self) -> &mut TaskDefaults {
        &mut self.defaults
    }

    /// Get task info.
    pub fn info(&self) -> &TaskInfo {
        &self.info
    }

    /// Get mutable task info.
    pub fn info_mut(&mut self) -> &mut TaskInfo {
        &mut self.info
    }

    /// Set the fanout (maximum concurrent workers/processes).
    pub fn set_fanout(&mut self, fanout: usize) {
        self.info.fanout = fanout;
    }

    /// Get the current fanout.
    pub fn fanout(&self) -> usize {
        self.info.fanout
    }

    // -----------------------------------------------------------------------
    // Topology
    // -----------------------------------------------------------------------

    /// Load a propagation topology from a topology configuration file.
    ///
    /// On success, stores the parser (which caches the tree internally).
    pub fn load_topology(&mut self, path: &str) -> Result<()> {
        let hostname = get_short_hostname();
        let mut parser = TopologyParser::new();
        parser
            .load(path)
            .map_err(|e| TaskError::Topology(format!("failed to load topology: {}", e)))?;
        // Validate that we can build the tree for our hostname
        parser
            .tree(&hostname)
            .map_err(|e| TaskError::Topology(format!("failed to build tree: {}", e)))?;
        self.topology_parser = Some(parser);
        Ok(())
    }

    /// Try to auto-load topology from default config paths.
    fn try_load_default_topology(&mut self) -> bool {
        if self.topology_parser.is_some() {
            return true;
        }
        let configs = topology_configs();
        for path in configs.iter().rev() {
            if path.exists() {
                if let Ok(()) = self.load_topology(&path.to_string_lossy()) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if tree mode is enabled by default.
    pub fn default_tree_is_enabled(&mut self) -> bool {
        if self.topology_parser.is_none() {
            self.try_load_default_topology();
        }
        self.topology_parser.is_some() && self.defaults.auto_tree
    }

    /// Get or create the default PropagationTreeRouter.
    pub fn default_router(&mut self) -> Result<&PropagationTreeRouter> {
        if self.router.is_none() {
            let hostname = get_short_hostname();
            let parser = self
                .topology_parser
                .as_mut()
                .ok_or_else(|| TaskError::Topology("no topology loaded".to_string()))?;
            let topology = parser
                .tree(&hostname)
                .map_err(|e| TaskError::Topology(format!("failed to build tree: {}", e)))?;
            let root_ns = topology
                .root()
                .ok_or_else(|| TaskError::Topology("topology has no root".to_string()))?;
            let root_str = root_ns.nodeset.to_string();
            let fanout = self.info.fanout;
            let router = PropagationTreeRouter::new(&root_str, topology, fanout)
                .map_err(|e| TaskError::Topology(format!("router creation failed: {}", e)))?;
            self.router = Some(router);
        }
        Ok(self.router.as_ref().unwrap())
    }

    /// Check if a topology is loaded.
    pub fn has_topology(&self) -> bool {
        self.topology_parser.is_some()
    }

    // -----------------------------------------------------------------------
    // Worker scheduling
    // -----------------------------------------------------------------------

    /// Schedule a local shell command for execution.
    ///
    /// If `nodes` is None, executes locally on a single node.
    /// If `nodes` is Some, executes the command on each node in the set.
    ///
    /// Returns the worker ID assigned to this worker.
    pub fn shell(
        &mut self,
        command: &str,
        nodes: Option<&str>,
        handler: Option<Box<dyn EventHandler>>,
    ) -> usize {
        self.shell_with_opts(command, nodes, handler, None, false)
    }

    /// Schedule a shell command with full options.
    ///
    /// Returns the worker ID.
    pub fn shell_with_opts(
        &mut self,
        command: &str,
        nodes: Option<&str>,
        handler: Option<Box<dyn EventHandler>>,
        timeout: Option<Duration>,
        autoclose: bool,
    ) -> usize {
        let node_list: Vec<String> = match nodes {
            Some(ns) => {
                // Parse as NodeSet for expansion
                match NodeSet::parse(ns) {
                    Ok(nodeset) => nodeset.iter().collect::<Vec<String>>(),
                    Err(_) => {
                        // Fall back to treating it as a single node
                        vec![ns.to_string()]
                    }
                }
            }
            None => {
                // Local execution — use localhost
                vec!["localhost".to_string()]
            }
        };

        let fanout = self.info.fanout;
        let worker_timeout = timeout.or_else(|| {
            if self.info.command_timeout > 0.0 {
                Some(Duration::from_secs_f64(self.info.command_timeout))
            } else {
                None
            }
        });

        let mut worker = ExecWorker::new(node_list, command.to_string(), fanout, worker_timeout);
        worker = worker.with_stderr(self.defaults.stderr);

        self.schedule(Box::new(worker), handler, autoclose)
    }

    /// Schedule a pre-built worker for execution.
    ///
    /// The worker will be started when `resume()` or `run()` is called.
    /// Returns the worker ID.
    pub fn schedule(
        &mut self,
        worker: Box<dyn Worker>,
        handler: Option<Box<dyn EventHandler>>,
        autoclose: bool,
    ) -> usize {
        let worker_id = self.next_worker_id;
        self.next_worker_id += 1;

        // Wrap with a gathering handler
        let gathering = TaskGatheringHandler::new(worker_id, handler);
        let mut managed_worker = ManagedWorker {
            worker,
            update_task_rc: true,
            autoclose,
        };
        managed_worker.worker.set_handler(Box::new(gathering));

        self.workers.push((worker_id, managed_worker));
        worker_id
    }

    // -----------------------------------------------------------------------
    // Execution
    // -----------------------------------------------------------------------

    /// Run the task: start all scheduled workers and block until they complete.
    ///
    /// Optionally schedule a shell command first (like Python's `task.run(cmd, ...)`).
    ///
    /// Returns Ok(()) on success, or the first error encountered.
    pub fn run(&mut self, timeout: Option<Duration>) -> Result<()> {
        self.timeout = timeout;
        self.resume()
    }

    /// Run with a command: convenience for `shell(cmd) + run()`.
    pub fn run_command(
        &mut self,
        command: &str,
        nodes: Option<&str>,
        handler: Option<Box<dyn EventHandler>>,
        timeout: Option<Duration>,
    ) -> Result<usize> {
        let wid = self.shell(command, nodes, handler);
        self.timeout = timeout;
        self.resume()?;
        Ok(wid)
    }

    /// Resume task execution — start all pending workers and run until done.
    pub fn resume(&mut self) -> Result<()> {
        if self.state == TaskState::Running {
            return Err(TaskError::AlreadyRunning);
        }

        self.reset();
        self.state = TaskState::Running;

        // Start all pending workers, respecting fanout
        let mut _running_count = 0;
        let mut errors: Vec<TaskError> = Vec::new();

        // Start workers
        for (_wid, mw) in &mut self.workers {
            if mw.worker.state() == WorkerState::Pending {
                match mw.worker.start() {
                    Ok(()) => _running_count += 1,
                    Err(e) => errors.push(TaskError::Worker(e)),
                }
            }
        }

        // Poll workers until all are done
        let start = std::time::Instant::now();
        loop {
            let mut any_running = false;
            for (_wid, mw) in &mut self.workers {
                if !mw.autoclose && !mw.worker.is_done() {
                    // Drive the worker: read from its fds and check for exits
                    let fds = mw.worker.read_fds();
                    for fd in &fds {
                        let _ = mw.worker.handle_read(*fd);
                    }
                    if !mw.worker.is_done() {
                        any_running = true;
                    }
                }
            }
            if !any_running {
                break;
            }

            // Check task-level timeout
            if let Some(timeout) = self.timeout {
                if start.elapsed() >= timeout {
                    // Abort all running workers
                    for (_wid, mw) in &mut self.workers {
                        if !mw.worker.is_done() {
                            mw.worker.abort(true);
                        }
                    }
                    self.state = TaskState::Done;
                    return Err(TaskError::Timeout);
                }
            }

            // Small sleep to avoid busy-wait
            std::thread::sleep(Duration::from_millis(10));
        }

        // Collect results from all workers
        self.collect_results();

        self.state = TaskState::Done;

        if let Some(err) = errors.into_iter().next() {
            Err(err)
        } else {
            Ok(())
        }
    }

    /// Abort the task, stopping all running workers.
    pub fn abort(&mut self, kill: bool) {
        for (_wid, mw) in &mut self.workers {
            if !mw.worker.is_done() {
                mw.worker.abort(kill);
            }
        }
        self.state = TaskState::Aborted;
    }

    /// Check if the task is currently running.
    pub fn running(&self) -> bool {
        self.state == TaskState::Running
    }

    /// Get the current task state.
    pub fn state(&self) -> TaskState {
        self.state
    }

    // -----------------------------------------------------------------------
    // Result collection (internal)
    // -----------------------------------------------------------------------

    /// Reset result buffers for a new run.
    fn reset(&mut self) {
        if let Some(ref mut tree) = self.stdout_tree {
            tree.clear();
        }
        if let Some(ref mut tree) = self.stderr_tree {
            tree.clear();
        }
        self.d_source_rc.clear();
        self.d_rc_sources.clear();
        self.max_rc = None;
        self.timeout_sources.clear();
    }

    /// Collect results from all workers' gathering handlers into the task stores.
    fn collect_results(&mut self) {
        for (wid, mw) in &mut self.workers {
            let worker_id = *wid;

            // Collect return codes from the worker's retcodes map
            if mw.update_task_rc {
                for (node, &rc) in mw.worker.retcodes() {
                    let source: Source = (worker_id, node.clone());
                    self.d_source_rc.insert(source.clone(), rc);
                    self.d_rc_sources.entry(rc).or_default().insert(source);

                    if self.max_rc.is_none_or(|max| rc > max) {
                        self.max_rc = Some(rc);
                    }
                }
            }

            // Extract buffered stdout/stderr/timeouts from the gathering handler.
            // take_handler() removes the handler from the worker, then we call
            // take_buffers() to drain the collected data into our MsgTrees.
            if let Some(mut handler) = mw.worker.take_handler() {
                let WorkerBuffers {
                    stdout,
                    stderr,
                    timeouts,
                } = handler.take_buffers();

                // Populate stdout MsgTree
                if let Some(ref mut tree) = self.stdout_tree {
                    for (node, chunks) in stdout {
                        let source: Source = (worker_id, node);
                        for chunk in chunks {
                            tree.add(source.clone(), chunk);
                        }
                    }
                }

                // Populate stderr MsgTree
                if let Some(ref mut tree) = self.stderr_tree {
                    for (node, chunks) in stderr {
                        let source: Source = (worker_id, node);
                        for chunk in chunks {
                            tree.add(source.clone(), chunk);
                        }
                    }
                }

                // Record timed-out nodes
                for node in timeouts {
                    self.timeout_sources.insert((worker_id, node));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Result accessors
    // -----------------------------------------------------------------------

    /// Get the maximum return code from the last run, or None if:
    /// - All commands timed out.
    /// - No command-based worker was executed.
    pub fn max_retcode(&self) -> Option<i32> {
        self.max_rc
    }

    /// Get the return code for a specific node.
    ///
    /// When the node is associated with multiple workers, returns the max
    /// return code from those workers. Returns None if not found.
    pub fn node_retcode(&self, node: &str) -> Option<i32> {
        let mut max_rc: Option<i32> = None;
        for ((_, n), &rc) in &self.d_source_rc {
            if n == node {
                max_rc = Some(max_rc.map_or(rc, |m: i32| m.max(rc)));
            }
        }
        max_rc
    }

    /// Iterate over return codes and their associated node lists.
    ///
    /// Returns an iterator of `(rc, Vec<node_name>)` tuples.
    /// If `match_keys` is Some, only includes matching nodes.
    pub fn iter_retcodes(&self, match_keys: Option<&HashSet<String>>) -> Vec<(i32, Vec<String>)> {
        let mut result = Vec::new();
        for (&rc, sources) in &self.d_rc_sources {
            let nodes: Vec<String> = sources
                .iter()
                .filter(|(_, n)| match_keys.is_none_or(|keys| keys.contains(n)))
                .map(|(_, n)| n.clone())
                .collect();
            if !nodes.is_empty() {
                result.push((rc, nodes));
            }
        }
        result
    }

    /// Iterate over return codes for a specific worker.
    pub fn iter_retcodes_by_worker(
        &self,
        worker_id: usize,
        match_keys: Option<&HashSet<String>>,
    ) -> Vec<(i32, Vec<String>)> {
        let mut result = Vec::new();
        for (&rc, sources) in &self.d_rc_sources {
            let nodes: Vec<String> = sources
                .iter()
                .filter(|(wid, n)| {
                    *wid == worker_id && match_keys.is_none_or(|keys| keys.contains(n))
                })
                .map(|(_, n)| n.clone())
                .collect();
            if !nodes.is_empty() {
                result.push((rc, nodes));
            }
        }
        result
    }

    /// Get the number of timed-out nodes.
    pub fn num_timeout(&self) -> usize {
        self.timeout_sources.len()
    }

    /// Iterate over timed-out nodes.
    pub fn iter_keys_timeout(&self) -> impl Iterator<Item = &str> {
        self.timeout_sources.iter().map(|(_, n)| n.as_str())
    }

    /// Get the number of timed-out nodes for a specific worker.
    pub fn num_timeout_by_worker(&self, worker_id: usize) -> usize {
        self.timeout_sources
            .iter()
            .filter(|(wid, _)| *wid == worker_id)
            .count()
    }

    /// Iterate over timed-out nodes for a specific worker.
    pub fn iter_keys_timeout_by_worker(&self, worker_id: usize) -> impl Iterator<Item = &str> {
        self.timeout_sources
            .iter()
            .filter(move |(wid, _)| *wid == worker_id)
            .map(|(_, n)| n.as_str())
    }

    // -----------------------------------------------------------------------
    // Buffer accessors (MsgTree-based)
    // -----------------------------------------------------------------------

    /// Get the stdout buffer for a specific node across all workers.
    ///
    /// Returns a concatenation of all stdout messages for that node.
    pub fn node_buffer(&self, node: &str) -> Result<Vec<u8>> {
        let tree = self
            .stdout_tree
            .as_ref()
            .ok_or_else(|| TaskError::MsgTreeDisabled("stdout".to_string()))?;
        let mut buf = Vec::new();
        for key in tree.keys() {
            if key.1 == node {
                if let Some(elem) = tree.get(key) {
                    let msg = elem.message();
                    if !buf.is_empty() {
                        buf.push(b'\n');
                    }
                    buf.extend_from_slice(&msg);
                }
            }
        }
        Ok(buf)
    }

    /// Get the stderr buffer for a specific node.
    pub fn node_error(&self, node: &str) -> Result<Vec<u8>> {
        let tree = self
            .stderr_tree
            .as_ref()
            .ok_or_else(|| TaskError::MsgTreeDisabled("stderr".to_string()))?;
        let mut buf = Vec::new();
        for key in tree.keys() {
            if key.1 == node {
                if let Some(elem) = tree.get(key) {
                    let msg = elem.message();
                    if !buf.is_empty() {
                        buf.push(b'\n');
                    }
                    buf.extend_from_slice(&msg);
                }
            }
        }
        Ok(buf)
    }

    /// Flush all stdout buffers.
    pub fn flush_buffers(&mut self) {
        if let Some(ref mut tree) = self.stdout_tree {
            tree.clear();
        }
    }

    /// Flush all stderr buffers.
    pub fn flush_errors(&mut self) {
        if let Some(ref mut tree) = self.stderr_tree {
            tree.clear();
        }
    }

    // -----------------------------------------------------------------------
    // Worker accessors
    // -----------------------------------------------------------------------

    /// Get the number of scheduled workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Get a reference to a worker by ID.
    pub fn worker(&self, worker_id: usize) -> Option<&dyn Worker> {
        self.workers
            .iter()
            .find(|(wid, _)| *wid == worker_id)
            .map(|(_, mw)| mw.worker.as_ref())
    }

    /// Get the return codes for a specific worker.
    pub fn worker_retcodes(&self, worker_id: usize) -> Option<&HashMap<String, i32>> {
        self.workers
            .iter()
            .find(|(wid, _)| *wid == worker_id)
            .map(|(_, mw)| mw.worker.retcodes())
    }
}

impl Default for Task {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // -- TaskState tests --

    #[test]
    fn test_task_state_variants() {
        assert_eq!(TaskState::Idle, TaskState::Idle);
        assert_eq!(TaskState::Running, TaskState::Running);
        assert_eq!(TaskState::Done, TaskState::Done);
        assert_eq!(TaskState::Aborted, TaskState::Aborted);
        assert_ne!(TaskState::Idle, TaskState::Running);
    }

    #[test]
    fn test_task_state_debug() {
        assert_eq!(format!("{:?}", TaskState::Idle), "Idle");
        assert_eq!(format!("{:?}", TaskState::Running), "Running");
        assert_eq!(format!("{:?}", TaskState::Done), "Done");
        assert_eq!(format!("{:?}", TaskState::Aborted), "Aborted");
    }

    #[test]
    fn test_task_state_clone_copy() {
        let s = TaskState::Running;
        let s2 = s;
        assert_eq!(s, s2);
    }

    // -- TaskError tests --

    #[test]
    fn test_task_error_display() {
        let e = TaskError::Timeout;
        assert_eq!(e.to_string(), "task timeout");

        let e = TaskError::AlreadyRunning;
        assert_eq!(e.to_string(), "task is already running");

        let e = TaskError::MsgTreeDisabled("stdout".to_string());
        assert_eq!(e.to_string(), "stdout_msgtree not set");

        let e = TaskError::General("test error".to_string());
        assert_eq!(e.to_string(), "task error: test error");

        let e = TaskError::Topology("no topology".to_string());
        assert_eq!(e.to_string(), "topology error: no topology");
    }

    #[test]
    fn test_task_error_from_worker_error() {
        let we = WorkerError::Timeout;
        let te: TaskError = we.into();
        assert!(matches!(te, TaskError::Worker(WorkerError::Timeout)));
    }

    // -- TaskDefaults tests --

    #[test]
    fn test_task_defaults_default() {
        let d = TaskDefaults::default();
        assert!(!d.stderr);
        assert!(d.stdin);
        assert!(d.stdout_msgtree);
        assert!(d.stderr_msgtree);
        assert!(!d.auto_tree);
    }

    #[test]
    fn test_task_defaults_from_defaults() {
        let defaults = Defaults::new();
        let d = TaskDefaults::from_defaults(&defaults);
        assert!(!d.stderr);
        assert!(d.stdin);
    }

    // -- TaskInfo tests --

    #[test]
    fn test_task_info_default() {
        let info = TaskInfo::default();
        assert!(!info.debug);
        assert_eq!(info.fanout, 64);
        assert_eq!(info.grooming_delay, 0.5);
        assert_eq!(info.connect_timeout, 10.0);
        assert_eq!(info.command_timeout, 0.0);
    }

    #[test]
    fn test_task_info_from_defaults() {
        let defaults = Defaults::new();
        let info = TaskInfo::from_defaults(&defaults);
        assert_eq!(info.fanout, defaults.fanout() as usize);
        assert_eq!(info.connect_timeout, defaults.connect_timeout());
        assert_eq!(info.command_timeout, defaults.command_timeout());
    }

    // -- Task construction tests --

    #[test]
    fn test_task_new() {
        let task = Task::new();
        assert_eq!(task.state(), TaskState::Idle);
        assert_eq!(task.num_workers(), 0);
        assert_eq!(task.max_retcode(), None);
        assert_eq!(task.num_timeout(), 0);
        assert!(!task.running());
    }

    #[test]
    fn test_task_default() {
        let task = Task::default();
        assert_eq!(task.state(), TaskState::Idle);
    }

    #[test]
    fn test_task_from_defaults() {
        let defaults = Defaults::new();
        let task = Task::from_defaults(&defaults);
        assert_eq!(task.state(), TaskState::Idle);
        assert_eq!(task.fanout(), defaults.fanout() as usize);
    }

    #[test]
    fn test_task_with_config() {
        let td = TaskDefaults {
            stderr: true,
            stdout_msgtree: false,
            ..Default::default()
        };

        let ti = TaskInfo {
            fanout: 32,
            ..Default::default()
        };

        let task = Task::with_config(td, ti);
        assert!(task.defaults().stderr);
        assert!(!task.defaults().stdout_msgtree);
        assert_eq!(task.fanout(), 32);
        // stdout_msgtree disabled → no stdout tree
        assert!(task.stdout_tree.is_none());
        assert!(task.stderr_tree.is_some());
    }

    // -- Fanout tests --

    #[test]
    fn test_task_set_fanout() {
        let mut task = Task::new();
        assert_eq!(task.fanout(), 64);
        task.set_fanout(128);
        assert_eq!(task.fanout(), 128);
    }

    // -- Worker scheduling tests --

    #[test]
    fn test_task_shell_local() {
        let mut task = Task::new();
        let wid = task.shell("echo hello", None, None);
        assert_eq!(wid, 0);
        assert_eq!(task.num_workers(), 1);
    }

    #[test]
    fn test_task_shell_remote() {
        let mut task = Task::new();
        let wid = task.shell("hostname", Some("node[1-3]"), None);
        assert_eq!(wid, 0);
        assert_eq!(task.num_workers(), 1);
    }

    #[test]
    fn test_task_shell_multiple() {
        let mut task = Task::new();
        let wid1 = task.shell("echo 1", None, None);
        let wid2 = task.shell("echo 2", None, None);
        assert_eq!(wid1, 0);
        assert_eq!(wid2, 1);
        assert_eq!(task.num_workers(), 2);
    }

    #[test]
    fn test_task_shell_with_opts() {
        let mut task = Task::new();
        let wid = task.shell_with_opts(
            "echo test",
            Some("node[1-5]"),
            None,
            Some(Duration::from_secs(30)),
            true,
        );
        assert_eq!(wid, 0);
        assert_eq!(task.num_workers(), 1);
    }

    // -- Execution tests --

    #[test]
    fn test_task_run_local_command() {
        let mut task = Task::new();
        task.shell("echo hello", None, None);
        let result = task.run(None);
        assert!(result.is_ok());
        assert_eq!(task.state(), TaskState::Done);
    }

    #[test]
    fn test_task_run_returns_retcode() {
        let mut task = Task::new();
        task.shell("true", None, None);
        task.run(None).unwrap();
        assert_eq!(task.max_retcode(), Some(0));
    }

    #[test]
    fn test_task_run_failing_command() {
        let mut task = Task::new();
        task.shell("false", None, None);
        task.run(None).unwrap();
        assert_eq!(task.max_retcode(), Some(1));
    }

    #[test]
    fn test_task_run_mixed_retcodes() {
        let mut task = Task::new();
        task.shell("true", None, None);
        task.shell("exit 42", None, None);
        task.run(None).unwrap();
        assert_eq!(task.max_retcode(), Some(42));
    }

    #[test]
    fn test_task_run_command_convenience() {
        let mut task = Task::new();
        let wid = task.run_command("echo test", None, None, None).unwrap();
        assert_eq!(wid, 0);
        assert_eq!(task.max_retcode(), Some(0));
    }

    #[test]
    fn test_task_run_multiple_nodes() {
        let mut task = Task::new();
        task.shell("echo hi", Some("node[1-3]"), None);
        task.run(None).unwrap();
        // Each "node" runs locally via ExecWorker (echo hi runs 3 times)
        assert_eq!(task.max_retcode(), Some(0));

        let retcodes = task.iter_retcodes(None);
        // All should be rc=0
        for (rc, _nodes) in &retcodes {
            assert_eq!(*rc, 0);
        }
    }

    // -- Retcode accessor tests --

    #[test]
    fn test_task_node_retcode() {
        let mut task = Task::new();
        task.shell("true", None, None);
        task.run(None).unwrap();
        // The node is "localhost" for local commands
        assert_eq!(task.node_retcode("localhost"), Some(0));
        assert_eq!(task.node_retcode("nonexistent"), None);
    }

    #[test]
    fn test_task_iter_retcodes_with_filter() {
        let mut task = Task::new();
        task.shell("echo hi", Some("node[1-3]"), None);
        task.run(None).unwrap();

        let mut filter = HashSet::new();
        filter.insert("node1".to_string());
        let retcodes = task.iter_retcodes(Some(&filter));
        for (_rc, nodes) in &retcodes {
            for n in nodes {
                assert_eq!(n, "node1");
            }
        }
    }

    #[test]
    fn test_task_iter_retcodes_by_worker() {
        let mut task = Task::new();
        let wid1 = task.shell("true", None, None);
        let wid2 = task.shell("exit 7", None, None);
        task.run(None).unwrap();

        let rc1 = task.iter_retcodes_by_worker(wid1, None);
        assert_eq!(rc1.len(), 1);
        assert_eq!(rc1[0].0, 0);

        let rc2 = task.iter_retcodes_by_worker(wid2, None);
        assert_eq!(rc2.len(), 1);
        assert_eq!(rc2[0].0, 7);
    }

    // -- Abort tests --

    #[test]
    fn test_task_abort() {
        let mut task = Task::new();
        task.shell("sleep 100", None, None);
        // Don't run, just abort
        task.abort(true);
        assert_eq!(task.state(), TaskState::Aborted);
    }

    // -- Timeout tests --

    #[test]
    fn test_task_run_with_timeout() {
        let mut task = Task::new();
        task.shell("sleep 100", None, None);
        let result = task.run(Some(Duration::from_millis(100)));
        assert!(result.is_err());
        match result.unwrap_err() {
            TaskError::Timeout => {}
            other => panic!("expected Timeout, got: {:?}", other),
        }
    }

    // -- Event handler chaining tests --

    #[test]
    fn test_task_with_custom_handler() {
        let started = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();
        let closed_clone = closed.clone();

        struct TestHandler {
            started: Arc<AtomicBool>,
            closed: Arc<AtomicBool>,
        }

        impl EventHandler for TestHandler {
            fn on_start(&mut self, _worker: &dyn Worker) {
                self.started.store(true, Ordering::SeqCst);
            }
            fn on_close(&mut self, _node: &str, _rc: i32) {
                self.closed.store(true, Ordering::SeqCst);
            }
        }

        let handler = TestHandler {
            started: started_clone,
            closed: closed_clone,
        };

        let mut task = Task::new();
        task.shell("echo hello", None, Some(Box::new(handler)));
        task.run(None).unwrap();

        assert!(started.load(Ordering::SeqCst), "on_start should be called");
        assert!(closed.load(Ordering::SeqCst), "on_close should be called");
    }

    // -- Reset tests --

    #[test]
    fn test_task_reset_between_runs() {
        let mut task = Task::new();
        task.shell("exit 5", None, None);
        task.run(None).unwrap();
        assert_eq!(task.max_retcode(), Some(5));

        // Schedule new work and run again
        task.shell("exit 3", None, None);
        // Reset state to allow re-run
        task.state = TaskState::Idle;
        task.run(None).unwrap();
        // Both workers are in the list. The first (exit 5) is Done and
        // won't re-run, but collect_results() still picks up its stored
        // retcodes. So max_rc is max(5, 3) = 5.
        assert_eq!(task.max_retcode(), Some(5));
    }

    // -- Topology tests --

    #[test]
    fn test_task_topology_none_by_default() {
        let task = Task::new();
        assert!(!task.has_topology());
    }

    #[test]
    fn test_task_default_tree_disabled() {
        let mut task = Task::new();
        assert!(!task.default_tree_is_enabled());
    }

    // -- Buffer accessor tests --

    #[test]
    fn test_task_node_buffer_empty() {
        let task = Task::new();
        let buf = task.node_buffer("somenode").unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_task_node_error_empty() {
        let task = Task::new();
        let buf = task.node_error("somenode").unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_task_node_buffer_disabled() {
        let td = TaskDefaults {
            stdout_msgtree: false,
            ..Default::default()
        };
        let task = Task::with_config(td, TaskInfo::default());
        let result = task.node_buffer("somenode");
        assert!(result.is_err());
    }

    #[test]
    fn test_task_flush_buffers() {
        let mut task = Task::new();
        task.flush_buffers();
        task.flush_errors();
        // Should not panic
    }

    // -- Worker accessor tests --

    #[test]
    fn test_task_worker_accessor() {
        let mut task = Task::new();
        let wid = task.shell("echo test", None, None);
        assert!(task.worker(wid).is_some());
        assert!(task.worker(999).is_none());
    }

    #[test]
    fn test_task_worker_retcodes_accessor() {
        let mut task = Task::new();
        let wid = task.shell("echo test", None, None);
        task.run(None).unwrap();
        let retcodes = task.worker_retcodes(wid);
        assert!(retcodes.is_some());
        assert_eq!(*retcodes.unwrap().get("localhost").unwrap(), 0);
    }

    // -- Info/Defaults mutability tests --

    #[test]
    fn test_task_defaults_mut() {
        let mut task = Task::new();
        assert!(!task.defaults().stderr);
        task.defaults_mut().stderr = true;
        assert!(task.defaults().stderr);
    }

    #[test]
    fn test_task_info_mut() {
        let mut task = Task::new();
        assert_eq!(task.info().fanout, 64);
        task.info_mut().fanout = 256;
        assert_eq!(task.info().fanout, 256);
    }

    // -- Hostname helper --

    #[test]
    fn test_get_short_hostname() {
        let hostname = get_short_hostname();
        assert!(!hostname.is_empty());
        assert!(!hostname.contains('.'));
    }

    // -- Gathering handler tests --

    #[test]
    fn test_gathering_handler_collects_retcodes() {
        let mut handler = TaskGatheringHandler::new(0, None);
        handler.on_close("node1", 0);
        handler.on_close("node2", 1);
        assert_eq!(handler.retcodes.len(), 2);
        assert_eq!(handler.retcodes["node1"], 0);
        assert_eq!(handler.retcodes["node2"], 1);
    }

    #[test]
    fn test_gathering_handler_collects_stdout() {
        let mut handler = TaskGatheringHandler::new(0, None);
        handler.on_read("node1", 1, b"hello");
        handler.on_read("node1", 1, b"world");
        handler.on_read("node2", 1, b"hi");
        assert_eq!(handler.stdout["node1"].len(), 2);
        assert_eq!(handler.stdout["node2"].len(), 1);
    }

    #[test]
    fn test_gathering_handler_collects_stderr() {
        let mut handler = TaskGatheringHandler::new(0, None);
        handler.on_read("node1", 2, b"error msg");
        assert_eq!(handler.stderr["node1"].len(), 1);
        assert!(handler.stdout.is_empty());
    }

    #[test]
    fn test_gathering_handler_collects_timeouts() {
        let mut handler = TaskGatheringHandler::new(0, None);
        handler.on_timeout("node1");
        handler.on_timeout("node2");
        assert_eq!(handler.timeouts.len(), 2);
        assert!(handler.timeouts.contains("node1"));
    }

    #[test]
    fn test_gathering_handler_chains_user_handler() {
        let called = Arc::new(AtomicUsize::new(0));
        let called_clone = called.clone();

        struct CountHandler(Arc<AtomicUsize>);
        impl EventHandler for CountHandler {
            fn on_close(&mut self, _node: &str, _rc: i32) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let user_handler = Box::new(CountHandler(called_clone));
        let mut handler = TaskGatheringHandler::new(0, Some(user_handler));
        handler.on_close("node1", 0);
        handler.on_close("node2", 0);

        assert_eq!(called.load(Ordering::SeqCst), 2);
        assert_eq!(handler.retcodes.len(), 2);
    }
}
