//! Canonical filesystem layout for the global and per-project brains.
//!
//! Every path the engine touches is derived from one of two roots:
//!
//! * the **global brain** at `~/.brain/` (shared across all projects), and
//! * the **project brain** at `<project_root>/.brain/` (scoped to one repo).
//!
//! Centralising the layout here means later phases (indexer, daemon, hooks)
//! never hand-build paths and therefore cannot drift out of sync.

use std::path::{Path, PathBuf};

use crate::error::{BrainError, Result};

/// Directory name used for the project brain (relative to the project root).
pub const PROJECT_BRAIN_DIR: &str = ".brain";
/// Directory name used for the global brain (relative to `$HOME`).
pub const GLOBAL_BRAIN_DIR: &str = ".brain";
/// File name of the per-project configuration committed next to the brain.
pub const PROJECT_CONFIG_FILE: &str = "brain.config.json";

/// Paths that make up the global brain at `~/.brain/`.
#[derive(Debug, Clone)]
pub struct GlobalPaths {
    /// Root of the global brain (`~/.brain`).
    pub root: PathBuf,
}

impl GlobalPaths {
    /// Resolve the global brain rooted at the current user's home directory.
    pub fn resolve() -> Result<Self> {
        let home = dirs::home_dir().ok_or(BrainError::HomeDirUnavailable)?;
        Ok(Self::with_home(home))
    }

    /// Build the layout for an explicit home directory (used by tests).
    pub fn with_home(home: impl AsRef<Path>) -> Self {
        Self {
            root: home.as_ref().join(GLOBAL_BRAIN_DIR),
        }
    }

    /// `~/.brain/config.json`
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }
    /// `~/.brain/providers.json`
    pub fn providers_file(&self) -> PathBuf {
        self.root.join("providers.json")
    }
    /// `~/.brain/cache`
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    /// `~/.brain/memory`
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }
    /// `~/.brain/logs`
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `~/.brain/models` — cached ONNX model files for local embeddings (Phase 3).
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// `~/.brain/llm_state.json` — persisted LLM availability state (rate-limit blocks).
    pub fn llm_state_file(&self) -> PathBuf {
        self.root.join("llm_state.json")
    }

    /// All directories that must exist for a healthy global brain.
    pub fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.root.clone(),
            self.cache_dir(),
            self.memory_dir(),
            self.logs_dir(),
            self.models_dir(),
        ]
    }
}

/// Paths that make up a project brain at `<project_root>/.brain/`.
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    /// The project root (where `brain.config.json` lives).
    pub root: PathBuf,
}

impl ProjectPaths {
    /// Anchor the layout at an explicit project root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<root>/.brain`
    pub fn brain_dir(&self) -> PathBuf {
        self.root.join(PROJECT_BRAIN_DIR)
    }
    /// `<root>/.brain/vectors` — reserved for the vector store (Phase 3).
    pub fn vectors_dir(&self) -> PathBuf {
        self.brain_dir().join("vectors")
    }
    /// `<root>/.brain/cache`
    pub fn cache_dir(&self) -> PathBuf {
        self.brain_dir().join("cache")
    }
    /// `<root>/.brain/summaries`
    pub fn summaries_dir(&self) -> PathBuf {
        self.brain_dir().join("summaries")
    }
    /// `<root>/.brain/logs`
    pub fn logs_dir(&self) -> PathBuf {
        self.brain_dir().join("logs")
    }
    /// `<root>/.brain/metadata.db` — relational metadata (SQLite).
    pub fn metadata_db(&self) -> PathBuf {
        self.brain_dir().join("metadata.db")
    }
    /// `<root>/.brain/brain.sock` — Unix domain socket for the daemon (Phase 7).
    pub fn socket_file(&self) -> PathBuf {
        self.brain_dir().join("brain.sock")
    }
    /// `<root>/.brain/brain.pid` — PID file written by the daemon (Phase 7).
    pub fn pid_file(&self) -> PathBuf {
        self.brain_dir().join("brain.pid")
    }
    /// `<root>/brain.config.json`
    pub fn config_file(&self) -> PathBuf {
        self.root.join(PROJECT_CONFIG_FILE)
    }
    /// `<root>/.gitignore`
    pub fn gitignore_file(&self) -> PathBuf {
        self.root.join(".gitignore")
    }

    /// All directories that must exist for a healthy project brain.
    pub fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.brain_dir(),
            self.vectors_dir(),
            self.cache_dir(),
            self.summaries_dir(),
            self.logs_dir(),
        ]
    }

    /// True if the project has already been initialised.
    pub fn is_initialised(&self) -> bool {
        self.metadata_db().exists() && self.config_file().exists()
    }
}
