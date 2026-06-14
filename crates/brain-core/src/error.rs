//! Error types shared across the Brain Engine core.
//!
//! A single [`BrainError`] enum is used so that callers (CLI, future daemon,
//! hooks) can match on well-defined variants instead of opaque strings.

use std::path::PathBuf;

/// Result alias used throughout `brain-core`.
pub type Result<T> = std::result::Result<T, BrainError>;

/// All fallible operations in the core return this error.
#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    /// The user's home directory could not be resolved (needed for the global brain).
    #[error("could not resolve the home directory for the current user")]
    HomeDirUnavailable,

    /// A path that was expected to be a directory is missing or is a file.
    #[error("path is not a usable directory: {0}")]
    NotADirectory(PathBuf),

    /// A configuration file exists but could not be parsed.
    #[error("invalid configuration in {path}: {source}")]
    ConfigParse {
        /// File that failed to parse.
        path: PathBuf,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },

    /// Wrapper around filesystem errors that keeps the offending path.
    #[error("filesystem error at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Wrapper around SQLite errors.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Serialization failure when writing a config file.
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    /// An include/exclude glob pattern in the project config is invalid.
    #[error("invalid glob pattern '{pattern}': {message}")]
    Glob {
        /// The offending pattern.
        pattern: String,
        /// Human-readable reason.
        message: String,
    },

    /// The filesystem walker failed.
    #[error("error walking project tree: {0}")]
    Walk(String),

    /// An embedding operation failed (model init, download, or inference).
    #[error("embedding error: {0}")]
    Embed(String),

    /// A vector store operation failed (LanceDB open, write, or query).
    #[error("vector store error: {0}")]
    Vector(String),
}

impl BrainError {
    /// Helper to attach a path to a raw [`std::io::Error`].
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        BrainError::Io {
            path: path.into(),
            source,
        }
    }
}
