//! Gitignore-aware project traversal with include/exclude filtering.
//!
//! The walker decides *which files are candidates* for indexing. It deliberately
//! does not read file contents (that happens in [`crate::index`]); it only needs
//! filesystem metadata, keeping no-op re-scans cheap.
//!
//! Filtering layers, in order:
//! 1. the `ignore` crate's `.gitignore` handling (honoured even without a `.git`
//!    directory, so it works in any folder);
//! 2. the project's `include_globs` / `exclude_globs` (the reliable secret and
//!    vendored-dependency filter);
//! 3. the `max_file_size_bytes` cap.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

use crate::config::ProjectConfig;
use crate::error::{BrainError, Result};

/// A file that survived all filters and is a candidate for indexing.
#[derive(Debug, Clone)]
pub struct WalkEntry {
    /// Project-relative path with forward slashes (stable across platforms).
    pub rel_path: String,
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time in unix seconds (0 if unavailable).
    pub mtime: i64,
}

/// Result of a walk: the candidate entries plus counts of what was filtered.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    /// Files that passed every filter.
    pub entries: Vec<WalkEntry>,
    /// Files skipped solely because they exceeded the size cap.
    pub skipped_large: usize,
    /// Files skipped by include/exclude globs.
    pub skipped_excluded: usize,
}

/// Walk `root`, returning indexing candidates according to `cfg`.
pub fn walk(root: &Path, cfg: &ProjectConfig) -> Result<WalkOutcome> {
    let includes = build_globset(&cfg.include_globs)?;
    let excludes = build_globset(&cfg.exclude_globs)?;

    let mut outcome = WalkOutcome::default();

    let walker = WalkBuilder::new(root)
        .hidden(false) // index dotfiles like `.github/`; secrets are excluded by glob
        .parents(false) // ignore ancestors' gitignores; only this project's rules
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false) // honour .gitignore even outside a git repo
        .build();

    for result in walker {
        let entry = result.map_err(|e| BrainError::Walk(e.to_string()))?;

        // Directories and symlink loops are not indexed.
        match entry.file_type() {
            Some(ft) if ft.is_file() => {}
            _ => continue,
        }

        let abs_path = entry.path().to_path_buf();
        let rel_path = match relative_slash(root, &abs_path) {
            Some(p) => p,
            None => continue, // outside root (shouldn't happen) — skip defensively
        };

        // Include must match and exclude must not.
        if !includes.is_match(&rel_path) || excludes.is_match(&rel_path) {
            outcome.skipped_excluded += 1;
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue, // unreadable metadata — skip rather than fail the run
        };

        if meta.len() > cfg.max_file_size_bytes {
            outcome.skipped_large += 1;
            continue;
        }

        outcome.entries.push(WalkEntry {
            rel_path,
            abs_path,
            size: meta.len(),
            mtime: mtime_secs(&meta),
        });
    }

    Ok(outcome)
}

/// Build a [`GlobSet`] from patterns, attaching the offending pattern on error.
fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|e| BrainError::Glob {
            pattern: pat.clone(),
            message: e.to_string(),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| BrainError::Glob {
        pattern: patterns.join(", "),
        message: e.to_string(),
    })
}

/// Compute a project-relative, forward-slashed path string.
fn relative_slash(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        parts.push(comp.as_os_str().to_string_lossy().into_owned());
    }
    Some(parts.join("/"))
}

/// Extract last-modified as unix seconds, or 0 when unavailable.
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
