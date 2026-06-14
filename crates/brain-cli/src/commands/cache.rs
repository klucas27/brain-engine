//! `brain cache` — inspect and manage the response cache.
//!
//! Subcommands:
//!   brain cache stats  — show the number of live (non-expired) entries
//!   brain cache clear  — delete all cache entries unconditionally
//!   brain cache purge  — delete only expired cache entries

use std::path::Path;

use clap::Subcommand;

use brain_core::cache;
use brain_core::db;
use brain_core::paths::ProjectPaths;
use brain_core::{BrainError, Result};

/// Available actions under `brain cache`.
#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Show the number of live (non-expired) cache entries.
    Stats,
    /// Delete ALL cache entries (both live and expired).
    Clear,
    /// Delete only expired cache entries, keeping live ones intact.
    Purge,
}

/// Entry point for `brain cache`.
pub fn run(root: &Path, json: bool, action: CacheAction) -> Result<()> {
    let project = ProjectPaths::new(root.to_path_buf());

    if !project.is_initialised() {
        return Err(BrainError::Walk(format!(
            "no brain found at {} — run `brain init` first",
            project.brain_dir().display()
        )));
    }

    let conn = db::open(&project.metadata_db())?;

    match action {
        CacheAction::Stats => {
            let live = cache::count_live(&conn)?;
            if json {
                println!("{}", serde_json::json!({ "live_entries": live }));
            } else {
                println!("Cache: {live} live entry/entries");
            }
        }
        CacheAction::Clear => {
            let removed = cache::clear(&conn)?;
            if json {
                println!("{}", serde_json::json!({ "removed": removed }));
            } else {
                println!("Cache cleared: {removed} entry/entries removed");
            }
        }
        CacheAction::Purge => {
            let removed = cache::purge_expired(&conn)?;
            if json {
                println!("{}", serde_json::json!({ "removed": removed }));
            } else {
                println!("Purged: {removed} expired entry/entries removed");
            }
        }
    }

    Ok(())
}
