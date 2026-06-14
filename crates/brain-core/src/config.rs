//! Configuration model for the global and per-project brains.
//!
//! All config is plain JSON so it is trivially inspectable and editable by the
//! user. Every struct derives sensible [`Default`] values, so a fresh install
//! works with zero manual editing while still being fully overridable.
//!
//! Load/save helpers are *create-if-missing*: reading a config that does not
//! exist yet returns the default and never errors, which keeps `brain init`
//! idempotent.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{BrainError, Result};

/// Schema version for the global config. Bumped when the shape changes so that
/// future migrations can detect and upgrade old files.
pub const GLOBAL_CONFIG_VERSION: u32 = 1;
/// Schema version for the per-project config.
pub const PROJECT_CONFIG_VERSION: u32 = 1;

// --------------------------------------------------------------------------
// Global config (~/.brain/config.json)
// --------------------------------------------------------------------------

/// Top-level global configuration shared across every project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// Schema version of this file.
    pub version: u32,
    /// Default embedding provider key (must exist in `providers.json`).
    pub default_embedding: String,
    /// Default LLM provider key (must exist in `providers.json`).
    pub default_llm: String,
    /// Thresholds used by the dynamic local-vs-API decision engine (Phase 5).
    pub decision: DecisionConfig,
    /// Cache behaviour (Phase 6).
    pub cache: CacheConfig,
    /// Log verbosity: `error` | `warn` | `info` | `debug` | `trace`.
    pub log_level: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            version: GLOBAL_CONFIG_VERSION,
            default_embedding: "local".to_string(),
            default_llm: "claude".to_string(),
            decision: DecisionConfig::default(),
            cache: CacheConfig::default(),
            log_level: "info".to_string(),
        }
    }
}

/// Deterministic thresholds for the dynamic decision engine.
///
/// The engine compares live system load against these values; keeping them in
/// config (rather than hard-coded) makes the routing behaviour reproducible and
/// tunable per machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    /// CPU usage (percent) at or above which work is pushed to the API.
    pub cpu_high_threshold: u8,
    /// Resident memory (MB) at or above which work is pushed to the API.
    pub memory_high_threshold_mb: u64,
    /// Batch size at or above which local processing is preferred (throughput).
    pub large_batch_threshold: usize,
    /// How long (in hours) to block Claude after a detected rate-limit error.
    /// Matches Claude's approximate token-quota reset cycle (~5 h).
    /// The block is stored in `~/.brain/llm_state.json` and expires automatically.
    pub claude_window_hours: u64,
}

impl Default for DecisionConfig {
    fn default() -> Self {
        Self {
            cpu_high_threshold: 80,
            memory_high_threshold_mb: 2048,
            large_batch_threshold: 64,
            claude_window_hours: 5,  // matches Claude's ~5 h rate-limit reset cycle
        }
    }
}

/// Cache configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Time-to-live for cached entries, in seconds. Defaults to 7 days.
    pub ttl_seconds: u64,
    /// Whether the (riskier) semantic cache is enabled. Off by default so the
    /// system never returns a fuzzy-matched, potentially-wrong answer unless
    /// the user opts in.
    pub semantic_enabled: bool,
    /// Cosine-similarity threshold above which a semantic cache hit is accepted.
    pub semantic_threshold: f32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 7 * 24 * 60 * 60,
            semantic_enabled: false,
            semantic_threshold: 0.95,
        }
    }
}

// --------------------------------------------------------------------------
// Providers (~/.brain/providers.json)
// --------------------------------------------------------------------------

/// Registry of available embedding and LLM providers.
///
/// Stored separately from `config.json` because it tends to contain
/// environment-specific endpoints/keys and changes on a different cadence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Providers {
    /// Embedding providers keyed by name (e.g. `local`, `deepseek`, `openai`).
    pub embedding: serde_json::Map<String, serde_json::Value>,
    /// LLM providers keyed by name (e.g. `claude`, `deepseek`).
    pub llm: serde_json::Map<String, serde_json::Value>,
}

impl Default for Providers {
    fn default() -> Self {
        let embedding = serde_json::json!({
            "local":    { "model": "bge-small-en-v1.5", "dim": 384, "runtime": "cpu" },
            "deepseek": { "api_base": "https://api.deepseek.com", "model": "deepseek-embedding", "api_key_env": "DEEPSEEK_API_KEY" },
            "openai":   { "api_base": "https://api.openai.com/v1", "model": "text-embedding-3-small", "dim": 1536, "api_key_env": "OPENAI_API_KEY" }
        });
        let llm = serde_json::json!({
            "claude":   { "via": "claude-code" },
            "deepseek": { "api_base": "https://api.deepseek.com", "model": "deepseek-chat", "api_key_env": "DEEPSEEK_API_KEY" }
        });
        Self {
            embedding: embedding.as_object().cloned().unwrap_or_default(),
            llm: llm.as_object().cloned().unwrap_or_default(),
        }
    }
}

// --------------------------------------------------------------------------
// Project config (<root>/brain.config.json)
// --------------------------------------------------------------------------

/// Per-project configuration committed alongside the project brain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Schema version of this file.
    pub version: u32,
    /// Human-readable project name (defaults to the root directory name).
    pub project_name: String,
    /// Embedding provider override for this project (falls back to global).
    pub embedding_provider: String,
    /// Glob patterns of files to include in indexing.
    pub include_globs: Vec<String>,
    /// Glob patterns to always exclude (vendored deps, build output, secrets).
    pub exclude_globs: Vec<String>,
    /// Files larger than this many bytes are skipped during indexing.
    pub max_file_size_bytes: u64,
    /// Chunking parameters used by the indexer (Phase 2).
    pub chunk: ChunkConfig,
}

impl ProjectConfig {
    /// Build a default config whose `project_name` is derived from `root`.
    pub fn for_root(root: &Path) -> Self {
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        Self {
            project_name: name,
            ..Self::default()
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            version: PROJECT_CONFIG_VERSION,
            project_name: "project".to_string(),
            embedding_provider: "local".to_string(),
            include_globs: vec!["**/*".to_string()],
            exclude_globs: vec![
                "**/.git/**".to_string(),
                "**/.brain/**".to_string(),
                // The brain's own config is noise, not project content.
                "brain.config.json".to_string(),
                "**/brain.config.json".to_string(),
                "**/node_modules/**".to_string(),
                "**/target/**".to_string(),
                "**/dist/**".to_string(),
                "**/build/**".to_string(),
                "**/.venv/**".to_string(),
                "**/*.lock".to_string(),
                "**/*.min.*".to_string(),
                // Never index obvious secret material.
                "**/.env".to_string(),
                "**/.env.*".to_string(),
                "**/*.pem".to_string(),
                "**/*.key".to_string(),
            ],
            max_file_size_bytes: 1024 * 1024,
            chunk: ChunkConfig::default(),
        }
    }
}

/// Chunking parameters for the indexer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkConfig {
    /// Maximum number of source lines per chunk.
    pub max_lines: usize,
    /// Number of overlapping lines between consecutive chunks (context bleed).
    pub overlap_lines: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_lines: 120,
            overlap_lines: 20,
        }
    }
}

// --------------------------------------------------------------------------
// Generic load / save helpers
// --------------------------------------------------------------------------

/// Read a JSON config of type `T`, returning [`Default`] if the file is absent.
///
/// A present-but-malformed file is a hard error (we do not silently overwrite
/// the user's hand edits).
pub fn load_or_default<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice::<T>(&bytes).map_err(|source| BrainError::ConfigParse {
                path: path.to_path_buf(),
                source,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(BrainError::io(path, e)),
    }
}

/// Write `value` as pretty JSON, creating parent directories as needed.
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BrainError::io(parent, e))?;
    }
    let mut json = serde_json::to_vec_pretty(value)?;
    json.push(b'\n');
    std::fs::write(path, json).map_err(|e| BrainError::io(path, e))
}

/// Write `value` only if `path` does not already exist. Returns `true` when a
/// file was created, `false` when an existing file was left untouched. This is
/// the primitive that makes `brain init` non-destructive and idempotent.
pub fn save_if_absent<T: Serialize>(path: &Path, value: &T) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    save(path, value)?;
    Ok(true)
}
