//! LLM availability state — persisted rate-limit tracking.
//!
//! When Claude Code signals that the Claude API is rate-limited (tokens
//! exhausted for the current ~5 h window), the brain records a
//! `blocked_until` Unix timestamp in `~/.brain/llm_state.json`.
//!
//! The router checks this file on every request:
//! * `blocked_until > now`  → Claude is still blocked → use DeepSeek.
//! * `blocked_until ≤ now`  → block has expired → use Claude again.
//!
//! The file is intentionally human-readable JSON so users can inspect or
//! manually override the block with a text editor.
//!
//! # File format
//! ```json
//! {
//!   "claude": { "blocked_until": 1718399400, "reason": "rate_limit" }
//! }
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{BrainError, Result};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Per-provider block entry stored in `llm_state.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBlock {
    /// Unix timestamp (seconds) after which the block expires.
    pub blocked_until: i64,
    /// Human-readable reason for the block (e.g. `"rate_limit"`).
    pub reason: String,
}

/// Full contents of `~/.brain/llm_state.json`.
///
/// Keys are provider names (`"claude"`, `"deepseek"`, …).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmState(pub HashMap<String, ProviderBlock>);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read `llm_state.json` from `path`, returning an empty state if missing.
///
/// A present-but-malformed file returns an error so the user is alerted to a
/// problem with their hand-edited state file rather than silently resetting it.
pub fn read(path: &Path) -> Result<LlmState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<LlmState>(&bytes).map_err(|source| {
            BrainError::ConfigParse {
                path: path.to_path_buf(),
                source,
            }
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LlmState::default()),
        Err(e) => Err(BrainError::io(path, e)),
    }
}

/// Persist `state` to `path` as pretty-printed JSON.
pub fn write(path: &Path, state: &LlmState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BrainError::io(parent, e))?;
    }
    let mut json = serde_json::to_vec_pretty(state)?;
    json.push(b'\n');
    std::fs::write(path, json).map_err(|e| BrainError::io(path, e))
}

/// Returns `true` if `provider` is currently blocked (block has not yet expired).
pub fn is_blocked(state: &LlmState, provider: &str) -> bool {
    match state.0.get(provider) {
        Some(block) => block.blocked_until > now_secs(),
        None => false,
    }
}

/// Block `provider` for `window_secs` seconds, writing the updated state to `path`.
///
/// If the provider is already blocked, the `blocked_until` timestamp is refreshed
/// to `now + window_secs` (re-arming the block).
pub fn block(path: &Path, provider: &str, window_secs: u64, reason: &str) -> Result<()> {
    let mut state = read(path)?;
    let blocked_until = now_secs() + window_secs as i64;
    state.0.insert(
        provider.to_string(),
        ProviderBlock {
            blocked_until,
            reason: reason.to_string(),
        },
    );
    write(path, &state)
}

/// Remove the block for `provider`, writing the updated state to `path`.
///
/// No-op if the provider was not blocked.
pub fn unblock(path: &Path, provider: &str) -> Result<()> {
    let mut state = read(path)?;
    state.0.remove(provider);
    write(path, &state)
}

/// Return seconds remaining until `provider`'s block expires, or `None` if not blocked.
pub fn secs_remaining(state: &LlmState, provider: &str) -> Option<i64> {
    state.0.get(provider).and_then(|b| {
        let rem = b.blocked_until - now_secs();
        if rem > 0 { Some(rem) } else { None }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn state_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("llm_state.json")
    }

    #[test]
    fn missing_file_returns_empty_state() {
        let dir = tempdir().unwrap();
        let state = read(&state_path(&dir)).unwrap();
        assert!(state.0.is_empty());
    }

    #[test]
    fn block_writes_and_is_detected() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        block(&path, "claude", 18_000, "rate_limit").unwrap();
        let state = read(&path).unwrap();
        assert!(is_blocked(&state, "claude"));
        assert!(!is_blocked(&state, "deepseek")); // unrelated provider
    }

    #[test]
    fn unblock_clears_entry() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        block(&path, "claude", 18_000, "rate_limit").unwrap();
        unblock(&path, "claude").unwrap();
        let state = read(&path).unwrap();
        assert!(!is_blocked(&state, "claude"));
    }

    #[test]
    fn expired_block_is_not_blocked() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        // Write a block that expired 1 second ago.
        let state = LlmState({
            let mut m = HashMap::new();
            m.insert(
                "claude".to_string(),
                ProviderBlock {
                    blocked_until: now_secs() - 1,
                    reason: "rate_limit".to_string(),
                },
            );
            m
        });
        write(&path, &state).unwrap();
        let loaded = read(&path).unwrap();
        assert!(!is_blocked(&loaded, "claude"));
    }

    #[test]
    fn secs_remaining_returns_none_when_not_blocked() {
        let state = LlmState::default();
        assert!(secs_remaining(&state, "claude").is_none());
    }

    #[test]
    fn secs_remaining_returns_positive_when_blocked() {
        let dir = tempdir().unwrap();
        let path = state_path(&dir);
        block(&path, "claude", 18_000, "rate_limit").unwrap();
        let state = read(&path).unwrap();
        let rem = secs_remaining(&state, "claude").unwrap();
        assert!(rem > 0 && rem <= 18_000);
    }
}
