//! Core types for DAG task execution.

use std::fmt;

use crate::dag::context::DagContext;

/// Unique identifier for a task in the DAG.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct TaskId(pub String);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> Self {
        TaskId(s.to_string())
    }
}

impl From<String> for TaskId {
    fn from(s: String) -> Self {
        TaskId(s)
    }
}

impl From<&String> for TaskId {
    fn from(s: &String) -> Self {
        TaskId(s.clone())
    }
}

/// The outcome of executing a single task.
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Task succeeded.
    Success,
    /// Task was skipped (cache hit).
    Skipped,
    /// Task failed with an error message.
    Failed(String),
}

impl TaskOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, TaskOutcome::Success | TaskOutcome::Skipped)
    }
}

/// Error handling policy for the DAG executor.
#[derive(Debug, Clone, Default)]
pub enum ErrorPolicy {
    /// Stop all execution on first failure.
    FailFast,
    /// Cancel tasks that depend on the failed task, continue independent branches.
    #[default]
    ContinueIndependent,
    /// Retry up to N times, then apply fallback policy.
    Retry {
        max_retries: u32,
        fallback: Box<ErrorPolicy>,
    },
}

/// Concurrency limit for a group of tasks.
#[derive(Debug, Clone)]
pub struct ConcurrencyLimit {
    /// Maximum concurrent tasks in this group. None = unlimited.
    pub max_concurrent: Option<usize>,
}

/// The core trait that all DAG tasks implement.
///
/// Tasks receive a shared `DagContext` for reading predecessor outputs
/// and writing their own output. The executor dispatches tasks to worker
/// threads and collects outcomes.
pub trait DagTask: Send + 'static {
    /// Execute this task.
    fn execute(&self, ctx: &DagContext) -> TaskOutcome;

    /// Human-readable description for logging/monitoring.
    fn describe(&self) -> String;

    /// Optional resource tag for worker pool slot management.
    /// Tasks with a resource tag require a matching slot in the pool.
    fn resource_tag(&self) -> Option<&str> {
        None
    }
}

/// A task that runs a shell command.
pub struct ShellTask {
    pub command: String,
    pub description: String,
    pub resource: Option<String>,
}

impl DagTask for ShellTask {
    fn execute(&self, _ctx: &DagContext) -> TaskOutcome {
        use std::process::Command;

        let output = match Command::new("sh").arg("-c").arg(&self.command).output() {
            Ok(o) => o,
            Err(e) => return TaskOutcome::Failed(format!("spawn failed: {}", e)),
        };

        if output.status.success() {
            TaskOutcome::Success
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            TaskOutcome::Failed(format!(
                "exit code {}: {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ))
        }
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn resource_tag(&self) -> Option<&str> {
        self.resource.as_deref()
    }
}

/// A task that runs a closure.
pub struct FnTask<F>
where
    F: Fn(&DagContext) -> TaskOutcome + Send + 'static,
{
    func: F,
    description: String,
}

impl<F> FnTask<F>
where
    F: Fn(&DagContext) -> TaskOutcome + Send + 'static,
{
    pub fn new(description: impl Into<String>, func: F) -> Self {
        Self {
            func,
            description: description.into(),
        }
    }
}

impl<F> DagTask for FnTask<F>
where
    F: Fn(&DagContext) -> TaskOutcome + Send + 'static,
{
    fn execute(&self, ctx: &DagContext) -> TaskOutcome {
        (self.func)(ctx)
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}
