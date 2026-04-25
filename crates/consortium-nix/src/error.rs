//! Error types for NixOS deployment operations.

use std::path::PathBuf;

use consortium::dag::DagError;

/// Errors from NixOS deployment operations.
#[derive(Debug, thiserror::Error)]
pub enum NixError {
    #[error("nix evaluation failed for {host}: {message}")]
    EvalFailed { host: String, message: String },

    #[error("nix build failed for {host}: {message}")]
    BuildFailed { host: String, message: String },

    #[error("closure copy failed to {host}: {message}")]
    CopyFailed { host: String, message: String },

    #[error("activation failed on {host}: {message}")]
    ActivationFailed { host: String, message: String },

    #[error("SSH connection failed to {host}: {message}")]
    SshFailed { host: String, message: String },

    #[error("builder {host} is unhealthy: {message}")]
    UnhealthyBuilder { host: String, message: String },

    #[error("no healthy builders available")]
    NoHealthyBuilders,

    #[error("configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to write machines file to {path}: {source}")]
    MachinesFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("{0}")]
    General(String),

    #[error("DAG execution error: {0}")]
    DagExecution(String),
}

impl From<DagError> for NixError {
    fn from(err: DagError) -> Self {
        NixError::DagExecution(err.to_string())
    }
}

impl Clone for NixError {
    fn clone(&self) -> Self {
        match self {
            NixError::EvalFailed { host, message } => NixError::EvalFailed {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::BuildFailed { host, message } => NixError::BuildFailed {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::CopyFailed { host, message } => NixError::CopyFailed {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::ActivationFailed { host, message } => NixError::ActivationFailed {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::SshFailed { host, message } => NixError::SshFailed {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::UnhealthyBuilder { host, message } => NixError::UnhealthyBuilder {
                host: host.clone(),
                message: message.clone(),
            },
            NixError::NoHealthyBuilders => NixError::NoHealthyBuilders,
            NixError::Config(e) => {
                // Clone ConfigError by converting to string and back
                NixError::General(format!("config error: {}", e))
            }
            NixError::Io(e) => NixError::Io(std::io::Error::new(e.kind(), e.to_string())),
            NixError::MachinesFile { path, source } => NixError::MachinesFile {
                path: path.clone(),
                source: std::io::Error::new(source.kind(), source.to_string()),
            },
            NixError::General(msg) => NixError::General(msg.clone()),
            NixError::DagExecution(msg) => NixError::DagExecution(msg.clone()),
        }
    }
}

pub type Result<T> = std::result::Result<T, NixError>;
