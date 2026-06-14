//! Relational metadata store (SQLite) for a project brain.
//!
//! This module owns the schema and a tiny, idempotent migration runner. The
//! schema is created in full here (Phase 1); later phases only *populate* it:
//!
//! * `files` / `chunks`  — indexer (Phase 2)
//! * `meta`              — pins the embedding model identity (Phase 3)
//! * `cache`            — response cache (Phase 6)
//! * `sessions` / `requests` — metrics (Phase 9)
//!
//! Creating the whole schema up front keeps later phases additive and avoids
//! scattering `CREATE TABLE` statements throughout the codebase.

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;

/// Current schema version. Each migration bumps `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 2;

/// Open (creating if needed) the metadata database at `path` and ensure the
/// schema is migrated to [`SCHEMA_VERSION`].
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| crate::error::BrainError::io(parent, e))?;
    }
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Apply connection-level pragmas tuned for a concurrent, embedded workload.
fn configure(conn: &Connection) -> Result<()> {
    // WAL allows the future watcher/daemon to read while the indexer writes.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Wait up to 5s on a locked DB instead of failing immediately (multi-session).
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Read the current schema version stored in `PRAGMA user_version`.
fn current_version(conn: &Connection) -> Result<i64> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v)
}

/// Run any outstanding migrations. Safe to call repeatedly.
pub fn migrate(conn: &Connection) -> Result<()> {
    let mut version = current_version(conn)?;
    if version < 1 {
        conn.execute_batch(MIGRATION_V1)?;
        conn.pragma_update(None, "user_version", 1)?;
        version = 1;
    }
    if version < 2 {
        conn.execute_batch(MIGRATION_V2)?;
        conn.pragma_update(None, "user_version", 2)?;
        version = 2;
    }
    debug_assert_eq!(version, SCHEMA_VERSION, "schema migrated to head");
    Ok(())
}

/// Initial schema. Forward-only; never edit a shipped migration — add a new one.
const MIGRATION_V1: &str = r#"
-- Key/value table for brain-wide invariants (e.g. pinned embedding model).
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- One row per indexed source file.
CREATE TABLE IF NOT EXISTS files (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,   -- project-relative path
    hash        TEXT NOT NULL,          -- content hash (sha256) for incremental reindex
    size_bytes  INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,       -- unix seconds
    lang        TEXT,                   -- detected language, nullable
    chunk_count INTEGER NOT NULL DEFAULT 0,
    indexed_at  INTEGER NOT NULL        -- unix seconds
);

-- One row per chunk produced from a file.
CREATE TABLE IF NOT EXISTS chunks (
    id            INTEGER PRIMARY KEY,
    file_id       INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL,     -- 0-based position within the file
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    content       TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0,
    vector_id     TEXT,                 -- id in the vector store, set in Phase 3
    created_at    INTEGER NOT NULL,
    UNIQUE(file_id, ordinal)
);
CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);

-- Response cache (exact + future semantic). Populated in Phase 6.
CREATE TABLE IF NOT EXISTS cache (
    key        TEXT PRIMARY KEY,        -- sha256 of normalised prompt+context
    response   TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER                  -- nullable; null = no TTL
);
CREATE INDEX IF NOT EXISTS idx_cache_expires ON cache(expires_at);

-- One row per Claude Code session. Populated in Phase 9.
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,       -- session uuid from the hook
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER
);

-- One row per intercepted request. Populated in Phase 9.
CREATE TABLE IF NOT EXISTS requests (
    id                       INTEGER PRIMARY KEY,
    session_id               TEXT REFERENCES sessions(id) ON DELETE SET NULL,
    timestamp                INTEGER NOT NULL,
    response_time_ms         INTEGER,
    context_tokens_estimated INTEGER,
    tokens_saved_estimated   INTEGER,
    chunks_used              INTEGER,
    retrieval_time_ms        INTEGER,
    embedding_source         TEXT,       -- 'local' | 'api'
    llm_used                 TEXT,
    cpu_usage_percent        REAL,
    memory_usage_mb          INTEGER,
    decision_reason          TEXT,
    cache_hit                INTEGER     -- 0/1
);
CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);
"#;

/// Migration V2 — Phase 6: add `query_vector` column to `cache` for semantic lookup.
///
/// `ALTER TABLE … ADD COLUMN` is always safe: existing rows get NULL, and the
/// column is nullable by design (exact-cache entries have no stored vector).
const MIGRATION_V2: &str = r#"
ALTER TABLE cache ADD COLUMN query_vector TEXT;  -- JSON array of f32, nullable
"#;

/// Convenience: count rows in `files` and `chunks` for `brain status`.
pub fn counts(conn: &Connection) -> Result<(i64, i64)> {
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
    let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    Ok((files, chunks))
}

/// Read a value from the `meta` table.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Upsert a value into the `meta` table.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}
