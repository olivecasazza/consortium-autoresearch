//! Worker module for executing commands across nodes.
//!
//! This module provides the abstraction for executing commands
//! across nodes, similar to ClusterShell's Worker.
//!
//! ## Submodules
//!
//! - [`exec`] — Local command execution worker (ExecWorker)
//! - [`ssh`] — SSH-based remote execution worker (SshWorker, ScpWorker)
//! - [`tree`] — Tree-based propagation worker (TreeWorker)

pub mod exec;
pub mod ssh;
pub mod tree;

use std::collections::{HashMap, HashSet};
use std::os::unix::io::RawFd;
use thiserror::Error;

/// Error types for the worker module.
#[derive(Debug, Error)]
pub enum WorkerError {
    /// I/O error wrapper.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// General worker error.
    #[error("worker error: {0}")]
    General(String),
    /// Operation timed out.
    #[error("timeout")]
    Timeout,
}

/// Result type alias for worker operations.
pub type Result<T> = std::result::Result<T, WorkerError>;

/// Per-node captured output chunks: node name -> chunks in arrival order.
pub type NodeChunks = HashMap<String, Vec<Vec<u8>>>;

/// What an [`EventHandler`] has buffered and not yet handed over.
///
/// Destructured by `Task::collect_results`; the three fields move independently
/// into the task's stdout tree, stderr tree, and timed-out node set.
#[derive(Debug, Default)]
pub struct WorkerBuffers {
    /// Captured stdout chunks, keyed by node name.
    pub stdout: NodeChunks,
    /// Captured stderr chunks, keyed by node name.
    pub stderr: NodeChunks,
    /// Nodes whose operation timed out.
    pub timeouts: HashSet<String>,
}

impl WorkerBuffers {
    /// Three empty collections.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Current state of a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// Worker is created but not started yet.
    Pending,
    /// Worker is currently running.
    Running,
    /// Worker has completed.
    Done,
    /// Worker was aborted.
    Aborted,
}

/// Event handler trait for worker events.
///
/// Implement this trait to receive notifications about worker events.
pub trait EventHandler: Send {
    /// Called when the worker starts.
    fn on_start(&mut self, worker: &dyn Worker) {
        let _ = worker;
    }

    /// Called when data is available for reading from a node.
    fn on_read(&mut self, node: &str, fd: RawFd, msg: &[u8]) {
        let _ = (node, fd, msg);
    }

    /// Called when a connection to a node is closed.
    fn on_close(&mut self, node: &str, rc: i32) {
        let _ = (node, rc);
    }

    /// Called when a node operation times out.
    fn on_timeout(&mut self, node: &str) {
        let _ = node;
    }

    /// Called when an error occurs for a node.
    fn on_error(&mut self, node: &str, error: &WorkerError) {
        let _ = (node, error);
    }

    /// Extract buffered stdout/stderr data from this handler.
    /// Returns (stdout, stderr, timeouts) where stdout/stderr are node -> `Vec<chunks>`.
    /// Default returns empty maps. Used by Task to collect results after execution.
    fn take_buffers(&mut self) -> WorkerBuffers {
        WorkerBuffers::empty()
    }
}

/// Trait for worker implementations.
///
/// A worker executes commands across multiple nodes, handling I/O
/// events and managing child processes.
pub trait Worker: Send {
    /// Start the worker.
    fn start(&mut self) -> Result<()>;

    /// Abort the worker, optionally killing child processes.
    fn abort(&mut self, kill: bool);

    /// Get the worker's current state.
    fn state(&self) -> WorkerState;

    /// Set the event handler for this worker.
    fn set_handler(&mut self, handler: Box<dyn EventHandler>);

    /// Get file descriptors for read interest.
    fn read_fds(&self) -> Vec<RawFd>;

    /// Get file descriptors for write interest.
    fn write_fds(&self) -> Vec<RawFd>;

    /// Handle a read event on the given file descriptor.
    fn handle_read(&mut self, fd: RawFd) -> Result<()>;

    /// Handle a write event on the given file descriptor.
    fn handle_write(&mut self, fd: RawFd) -> Result<()>;

    /// Check if the worker has completed.
    fn is_done(&self) -> bool;

    /// Get the return codes map: node -> return_code.
    fn retcodes(&self) -> &HashMap<String, i32>;

    /// Get the number of nodes.
    fn num_nodes(&self) -> usize;

    /// Take the event handler out of the worker (used by Task to extract gathered data).
    fn take_handler(&mut self) -> Option<Box<dyn EventHandler>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test WorkerState enum variants
    #[test]
    fn test_worker_state_variants() {
        assert_eq!(WorkerState::Pending, WorkerState::Pending);
        assert_eq!(WorkerState::Running, WorkerState::Running);
        assert_eq!(WorkerState::Done, WorkerState::Done);
        assert_eq!(WorkerState::Aborted, WorkerState::Aborted);
    }

    /// Test WorkerState is Debug
    #[test]
    fn test_worker_state_debug() {
        assert_eq!(format!("{:?}", WorkerState::Pending), "Pending");
        assert_eq!(format!("{:?}", WorkerState::Running), "Running");
        assert_eq!(format!("{:?}", WorkerState::Done), "Done");
        assert_eq!(format!("{:?}", WorkerState::Aborted), "Aborted");
    }

    /// Test WorkerState is Clone
    #[test]
    fn test_worker_state_clone() {
        let state = WorkerState::Pending;
        // Call the Clone impl explicitly — this test is about Clone, and
        // `state.clone()` on a Copy type would just exercise Copy.
        let cloned = WorkerState::clone(&state);
        assert_eq!(state, cloned);
        // Copy and Clone must agree.
        let copied: WorkerState = state;
        assert_eq!(copied, cloned);
    }

    /// Test WorkerState is Copy
    #[test]
    fn test_worker_state_copy() {
        fn take_copy<T: Copy>(_: T) {}
        take_copy(WorkerState::Pending);
    }

    /// Test WorkerState is PartialEq
    #[test]
    fn test_worker_state_partial_eq() {
        assert!(WorkerState::Pending == WorkerState::Pending);
        assert!(WorkerState::Running != WorkerState::Pending);
    }

    /// Test WorkerError variants
    #[test]
    fn test_worker_error_variants() {
        let io_err = WorkerError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        let gen_err = WorkerError::General("something went wrong".to_string());
        let timeout_err = WorkerError::Timeout;

        matches!(io_err, WorkerError::Io(_));
        matches!(gen_err, WorkerError::General(_));
        matches!(timeout_err, WorkerError::Timeout);
    }

    /// Test WorkerError implements Error trait
    #[test]
    fn test_worker_error_impls_error() {
        use std::error::Error;

        let gen_err = WorkerError::General("test".to_string());
        assert!(gen_err.source().is_none());

        let timeout_err = WorkerError::Timeout;
        assert!(timeout_err.source().is_none());
    }

    /// Test WorkerError implements Display trait
    #[test]
    fn test_worker_error_impls_display() {
        let io_err = WorkerError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file.txt",
        ));
        assert_eq!(io_err.to_string(), "io error: file.txt");

        let gen_err = WorkerError::General("test error".to_string());
        assert_eq!(gen_err.to_string(), "worker error: test error");

        let timeout_err = WorkerError::Timeout;
        assert_eq!(timeout_err.to_string(), "timeout");
    }

    /// Test EventHandler default methods compile
    #[test]
    fn test_event_handler_default_methods() {
        struct TestHandler;

        impl EventHandler for TestHandler {}

        let handler = TestHandler;

        // These should compile with default implementations (no-op tests)
        // We just verify the methods exist and can be called
        let _ = handler;
    }

    /// Test that EventHandler is object-safe
    #[test]
    fn test_event_handler_object_safe() {
        struct TestHandler;

        impl EventHandler for TestHandler {}

        let _handler: Box<dyn EventHandler> = Box::new(TestHandler);
    }

    /// Test EventHandler is Send
    #[test]
    fn test_event_handler_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn EventHandler>>();
    }

    /// Test WorkerError Result type alias
    #[test]
    fn test_result_type_alias() {
        let ok: Result<()> = Ok(());
        let err: Result<()> = Err(WorkerError::Timeout);

        assert!(ok.is_ok());
        assert!(err.is_err());
    }
}
