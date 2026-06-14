//! # brain-core
//!
//! Foundation library for the **Brain Engine** — a local AI context layer for
//! Claude Code.
//!
//! Phase 1 (foundation):
//! * [`paths`]   — canonical filesystem layout for the global/project brains
//! * [`config`]  — JSON configuration model with safe load/save helpers
//! * [`db`]      — SQLite metadata schema + idempotent migrations
//! * [`scaffold`] — idempotent `init` that wires the above together
//! * [`error`]   — shared error/result types
//!
//! Phase 2 (indexer):
//! * [`walk`]  — gitignore-aware traversal with include/exclude filtering
//! * [`chunk`] — deterministic, overlapping line-window chunking
//! * [`hash`]  — SHA-256 content hashing for incremental reindexing
//! * [`index`] — parallel, incremental indexer writing `files`/`chunks`
//!
//! Phase 3 (embeddings & vector store):
//! * [`vectors`] — LanceDB wrapper that persists chunk embeddings
//!
//! Phase 4 (retrieval & query):
//! * [`tokens`]   — lightweight token estimator for context budgeting
//! * [`retrieve`] — ANN search + SQLite metadata enrichment
//! * [`context`]  — assemble top-k chunks within a token budget
//!
//! Phase 5 (decision engine & AI router):
//! * [`decision`] — sample CPU/RAM, apply thresholds, choose local vs API embeddings
//! * [`decision`]   — sample CPU/RAM, apply thresholds, choose local vs API embeddings
//! * [`router`]     — route LLM requests (DeepSeek vs Claude) based on system load
//! * [`llm_state`]  — persist rate-limit blocks in `~/.brain/llm_state.json`
//!
//! Phase 6 (cache layer):
//! * [`cache`] — exact SHA-256 response cache with TTL; optional semantic cache
//!   via cosine similarity (opt-in, disabled by default)
//!
//! Phase 9 (metrics & logging):
//! * [`metrics`] — per-request recording, session tracking, JSON daily logs,
//!   and aggregated stats for `brain stats`

pub mod cache;
pub mod chunk;
pub mod config;
pub mod context;
pub mod db;
pub mod decision;
pub mod error;
pub mod hash;
pub mod index;
pub mod llm_state;
pub mod metrics;
pub mod paths;
pub mod retrieve;
pub mod router;
pub mod scaffold;
pub mod tokens;
pub mod vectors;
pub mod walk;

pub use error::{BrainError, Result};

/// Engine version, surfaced by `brain status`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolve a usable project root from an optional user-supplied path.
///
/// `None` means "use the current working directory". The path is canonicalised
/// so downstream relative-path logic is stable.
pub fn resolve_root(explicit: Option<&std::path::Path>) -> Result<std::path::PathBuf> {
    let raw = match explicit {
        Some(p) => p.to_path_buf(),
        None => {
            std::env::current_dir().map_err(|e| BrainError::io(std::path::PathBuf::from("."), e))?
        }
    };
    if !raw.is_dir() {
        return Err(BrainError::NotADirectory(raw));
    }
    // canonicalize resolves symlinks/`.`/`..`; fall back to the raw path if the
    // platform refuses (e.g. permission), since the dir is known to exist.
    Ok(raw.canonicalize().unwrap_or(raw))
}
