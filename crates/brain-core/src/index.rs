//! Incremental project indexer.
//!
//! Orchestrates [`walk`](crate::walk) + [`chunk`](crate::chunk) and persists the
//! result into the `files` / `chunks` tables. Design points:
//!
//! * **Incremental.** A fast path skips files whose size *and* mtime match the
//!   database without reading them, so a no-change re-index does ~zero work.
//!   Files that pass the fast path but whose content hash matches are also
//!   treated as unchanged (only their mtime is refreshed).
//! * **Parallel.** Reading, binary-detection, hashing and chunking run across
//!   cores via Rayon. Only the final SQLite writes are serial (single writer).
//! * **Self-healing.** Files deleted on disk are removed from the index, and a
//!   changed file's old chunks are replaced atomically.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rayon::prelude::*;
use rusqlite::Connection;

use crate::chunk::{chunk_text, Chunk};
use crate::config::ProjectConfig;
use crate::error::{BrainError, Result};
use crate::hash::sha256_hex;
use crate::walk::{self, WalkEntry};

/// Summary of an indexing run, suitable for CLI display and metrics.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    /// Candidate files returned by the walker (post size/glob filtering).
    pub scanned: usize,
    /// Files newly indexed or re-indexed because their content changed.
    pub indexed: usize,
    /// Files whose content was unchanged since the last run.
    pub unchanged: usize,
    /// Files removed from the index because they no longer exist on disk.
    pub removed: usize,
    /// Total chunks written during this run.
    pub chunks_written: usize,
    /// Candidate files skipped because they were detected as binary.
    pub skipped_binary: usize,
    /// Files skipped by the walker for exceeding the size cap.
    pub skipped_large: usize,
    /// Files skipped by the walker via include/exclude globs.
    pub skipped_excluded: usize,
}

/// Payload for a file whose content changed and must be (re)written.
struct ChangedFile {
    hash: String,
    size: u64,
    mtime: i64,
    lang: Option<String>,
    chunks: Vec<Chunk>,
}

/// Per-file outcome computed in parallel before the serial write phase.
enum FileWork {
    /// Size + mtime matched the DB; not read.
    Unchanged,
    /// Content hash matched; refresh mtime/size only.
    Touched { size: u64, mtime: i64 },
    /// Content changed (or new file); replace its chunks.
    Changed(ChangedFile),
    /// File is binary; excluded from the text index.
    SkippedBinary,
}

/// Snapshot of a file row used for change detection.
struct ExistingFile {
    id: i64,
    hash: String,
    size: u64,
    mtime: i64,
}

/// Index (or re-index) `root` into the metadata database `conn`.
pub fn index_project(
    root: &Path,
    cfg: &ProjectConfig,
    conn: &mut Connection,
) -> Result<IndexStats> {
    let outcome = walk::walk(root, cfg)?;
    let existing = load_existing(conn)?;

    // Phase A — parallel, read-only: classify every candidate file.
    let work: Vec<(String, Result<FileWork>)> = outcome
        .entries
        .par_iter()
        .map(|entry| (entry.rel_path.clone(), classify(entry, &existing, cfg)))
        .collect();

    // Surface the first error (if any) instead of silently dropping files.
    let mut classified: Vec<(String, FileWork)> = Vec::with_capacity(work.len());
    for (path, res) in work {
        classified.push((path, res?));
    }

    // Phase B — serial: persist within a single transaction.
    let seen: HashSet<&str> = classified.iter().map(|(p, _)| p.as_str()).collect();
    let now = now_secs();

    let mut stats = IndexStats {
        scanned: outcome.entries.len(),
        skipped_large: outcome.skipped_large,
        skipped_excluded: outcome.skipped_excluded,
        ..IndexStats::default()
    };

    let tx = conn.transaction()?;
    for (rel_path, work) in &classified {
        match work {
            FileWork::Unchanged => stats.unchanged += 1,
            FileWork::SkippedBinary => stats.skipped_binary += 1,
            FileWork::Touched { size, mtime } => {
                tx.execute(
                    "UPDATE files SET size_bytes = ?1, mtime = ?2, indexed_at = ?3 WHERE path = ?4",
                    rusqlite::params![*size as i64, mtime, now, rel_path],
                )?;
                stats.unchanged += 1;
            }
            FileWork::Changed(changed) => {
                write_changed(&tx, rel_path, changed, now)?;
                stats.indexed += 1;
                stats.chunks_written += changed.chunks.len();
            }
        }
    }

    // Remove files that vanished from disk (cascade drops their chunks).
    for (path, ef) in &existing {
        if !seen.contains(path.as_str()) {
            tx.execute("DELETE FROM files WHERE id = ?1", [ef.id])?;
            stats.removed += 1;
        }
    }

    tx.commit()?;
    Ok(stats)
}

/// Classify one file without touching the database connection.
fn classify(
    entry: &WalkEntry,
    existing: &HashMap<String, ExistingFile>,
    cfg: &ProjectConfig,
) -> Result<FileWork> {
    // Fast path: trust size + mtime to declare an unchanged file (no read).
    if let Some(prev) = existing.get(&entry.rel_path) {
        if prev.size == entry.size && prev.mtime == entry.mtime {
            return Ok(FileWork::Unchanged);
        }
    }

    let bytes = std::fs::read(&entry.abs_path).map_err(|e| BrainError::io(&entry.abs_path, e))?;

    if is_probably_binary(&bytes) {
        return Ok(FileWork::SkippedBinary);
    }
    // Binary check passed, so the content should be valid UTF-8 text.
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return Ok(FileWork::SkippedBinary),
    };

    let hash = sha256_hex(text.as_bytes());

    // Content identical despite a changed mtime: just refresh metadata.
    if let Some(prev) = existing.get(&entry.rel_path) {
        if prev.hash == hash {
            return Ok(FileWork::Touched {
                size: entry.size,
                mtime: entry.mtime,
            });
        }
    }

    let chunks = chunk_text(&text, &cfg.chunk);
    Ok(FileWork::Changed(ChangedFile {
        hash,
        size: entry.size,
        mtime: entry.mtime,
        lang: detect_language(&entry.rel_path),
        chunks,
    }))
}

/// Upsert a changed file and replace its chunks inside the open transaction.
fn write_changed(
    tx: &rusqlite::Transaction<'_>,
    rel_path: &str,
    changed: &ChangedFile,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO files(path, hash, size_bytes, mtime, lang, chunk_count, indexed_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(path) DO UPDATE SET
            hash = excluded.hash,
            size_bytes = excluded.size_bytes,
            mtime = excluded.mtime,
            lang = excluded.lang,
            chunk_count = excluded.chunk_count,
            indexed_at = excluded.indexed_at",
        rusqlite::params![
            rel_path,
            changed.hash,
            changed.size as i64,
            changed.mtime,
            changed.lang,
            changed.chunks.len() as i64,
            now
        ],
    )?;

    let file_id: i64 = tx.query_row("SELECT id FROM files WHERE path = ?1", [rel_path], |r| {
        r.get(0)
    })?;

    // Replace chunks wholesale: simplest correct strategy for a changed file.
    tx.execute("DELETE FROM chunks WHERE file_id = ?1", [file_id])?;

    let mut stmt = tx.prepare(
        "INSERT INTO chunks(file_id, ordinal, start_line, end_line, content,
                            content_hash, token_estimate, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for ch in &changed.chunks {
        stmt.execute(rusqlite::params![
            file_id,
            ch.ordinal as i64,
            ch.start_line as i64,
            ch.end_line as i64,
            ch.content,
            ch.content_hash,
            ch.token_estimate as i64,
            now,
        ])?;
    }
    Ok(())
}

/// Load the current `files` rows into a map keyed by path.
fn load_existing(conn: &Connection) -> Result<HashMap<String, ExistingFile>> {
    let mut stmt = conn.prepare("SELECT id, path, hash, size_bytes, mtime FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            ExistingFile {
                id: r.get(0)?,
                hash: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                mtime: r.get(4)?,
            },
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (path, ef) = row?;
        map.insert(path, ef);
    }
    Ok(map)
}

/// Heuristic binary detection: a NUL byte in the first 8 KiB marks the file as
/// binary. This mirrors what git does and is more reliable for "is this text we
/// can chunk?" than format sniffing.
fn is_probably_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(8192)];
    window.contains(&0)
}

/// Best-effort language label from a file's extension. `None` when unknown.
fn detect_language(rel_path: &str) -> Option<String> {
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "shell",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" => "css",
        _ => return None,
    };
    Some(lang.to_string())
}

/// Current unix time in seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
