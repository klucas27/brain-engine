//! Integration tests for Phase 4 — retrieval and context assembly.
//!
//! These tests exercise the full pipeline:
//!   VectorStore::upsert → retrieve::search → context::assemble
//!
//! They do **not** download any embedding model; vectors are constructed
//! synthetically.  The goal is to verify that:
//!   1. ANN search returns the closest chunk (cosine similarity).
//!   2. `enrich` correctly joins LanceDB hits with SQLite metadata.
//!   3. `context::assemble` respects the token budget.
//!   4. `tokens::project_total` sums the DB correctly.

use brain_core::context;
use brain_core::db;
use brain_core::paths::ProjectPaths;
use brain_core::retrieve;
use brain_core::scaffold;
use brain_core::tokens;
use brain_core::vectors::{ChunkVector, VectorStore};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Stand up an initialised project in a temp dir and return (TempDir, conn, vs).
fn setup() -> (tempfile::TempDir, rusqlite::Connection, VectorStore) {
    let tmp = tempfile::tempdir().unwrap();
    let project = ProjectPaths::new(tmp.path().to_path_buf());
    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();

    let conn = db::open(&project.metadata_db()).unwrap();
    let vs = VectorStore::open(&project.vectors_dir()).unwrap();
    (tmp, conn, vs)
}

/// Insert a synthetic chunk row into SQLite and return its `id`.
///
/// Matches the v1 schema in `db.rs`: `files` uses `mtime` (unix seconds);
/// `chunks` requires `ordinal` and `created_at`.
fn insert_chunk(
    conn: &rusqlite::Connection,
    path: &str,
    start_line: usize,
    end_line: usize,
    token_estimate: usize,
) -> i64 {
    // Insert (or reuse) the parent file row.
    conn.execute(
        "INSERT OR IGNORE INTO files
             (path, size_bytes, hash, mtime, indexed_at)
         VALUES (?1, 0, 'deadbeef', 0, 0)",
        rusqlite::params![path],
    )
    .unwrap();

    let file_id: i64 = conn
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            rusqlite::params![path],
            |r| r.get(0),
        )
        .unwrap();

    // Derive a unique ordinal by counting existing chunks for this file.
    let ordinal: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE file_id = ?1",
            rusqlite::params![file_id],
            |r| r.get(0),
        )
        .unwrap();

    conn.execute(
        "INSERT INTO chunks
             (file_id, ordinal, start_line, end_line, token_estimate,
              content_hash, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'abc', 'x', 0)",
        rusqlite::params![
            file_id,
            ordinal,
            start_line as i64,
            end_line as i64,
            token_estimate as i64
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn search_on_empty_store_returns_empty_vec() {
    let (_tmp, conn, vs) = setup();
    let query = vec![0.0f32; 4];
    let results = retrieve::search(&vs, &conn, &query, 5).unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_returns_most_similar_chunk_first() {
    let (_tmp, conn, vs) = setup();
    let dim = 4usize;

    // Three chunks with synthetic embeddings.
    // chunk A: points along [1, 0, 0, 0] — most similar to [1, 0, 0, 0].
    // chunk B: points along [0, 1, 0, 0] — less similar.
    // chunk C: points along [0, 0, 1, 0] — least similar.
    let id_a = insert_chunk(&conn, "src/a.rs", 1, 10, 20);
    let id_b = insert_chunk(&conn, "src/b.rs", 1, 10, 20);
    let id_c = insert_chunk(&conn, "src/c.rs", 1, 10, 20);

    vs.upsert(
        &[
            ChunkVector {
                chunk_id: id_a,
                vector: vec![1.0, 0.0, 0.0, 0.0],
                file_path: "src/a.rs".into(),
                content: "chunk a".into(),
            },
            ChunkVector {
                chunk_id: id_b,
                vector: vec![0.0, 1.0, 0.0, 0.0],
                file_path: "src/b.rs".into(),
                content: "chunk b".into(),
            },
            ChunkVector {
                chunk_id: id_c,
                vector: vec![0.0, 0.0, 1.0, 0.0],
                file_path: "src/c.rs".into(),
                content: "chunk c".into(),
            },
        ],
        dim,
    )
    .unwrap();

    let query = vec![1.0f32, 0.0, 0.0, 0.0]; // identical to chunk A
    let results = retrieve::search(&vs, &conn, &query, 3).unwrap();

    assert!(!results.is_empty(), "expected at least one result");
    // The top result should be chunk A.
    assert_eq!(
        results[0].chunk_id, id_a,
        "chunk A should rank first; got chunk_id={}",
        results[0].chunk_id
    );
    // Scores should be in descending order.
    for w in results.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "results not sorted by descending score: {} < {}",
            w[0].score,
            w[1].score
        );
    }
}

#[test]
fn search_enriches_with_sqlite_metadata() {
    let (_tmp, conn, vs) = setup();
    let dim = 4usize;

    let chunk_id = insert_chunk(&conn, "src/foo.rs", 42, 55, 13);

    vs.upsert(
        &[ChunkVector {
            chunk_id,
            vector: vec![1.0, 0.0, 0.0, 0.0],
            file_path: "src/foo.rs".into(),
            content: "content of foo".into(),
        }],
        dim,
    )
    .unwrap();

    let results = retrieve::search(&vs, &conn, &[1.0, 0.0, 0.0, 0.0], 1).unwrap();
    assert_eq!(results.len(), 1);

    let c = &results[0];
    assert_eq!(c.start_line, 42);
    assert_eq!(c.end_line, 55);
    assert_eq!(c.token_estimate, 13);
    assert_eq!(c.file_path, "src/foo.rs");
}

#[test]
fn citation_format_is_path_colon_lines() {
    let (_tmp, conn, vs) = setup();
    let dim = 4usize;

    let chunk_id = insert_chunk(&conn, "src/bar.rs", 7, 20, 5);
    vs.upsert(
        &[ChunkVector {
            chunk_id,
            vector: vec![1.0, 0.0, 0.0, 0.0],
            file_path: "src/bar.rs".into(),
            content: "bar content".into(),
        }],
        dim,
    )
    .unwrap();

    let results = retrieve::search(&vs, &conn, &[1.0, 0.0, 0.0, 0.0], 1).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(retrieve::citation(&results[0]), "src/bar.rs:7-20");
}

#[test]
fn context_assembly_respects_budget() {
    let (_tmp, conn, vs) = setup();
    let dim = 4usize;

    // Insert three chunks; the second won't fit in a tight budget.
    let id1 = insert_chunk(&conn, "a.rs", 1, 40, 400);
    let id2 = insert_chunk(&conn, "b.rs", 1, 40, 400);
    let id3 = insert_chunk(&conn, "c.rs", 1, 10, 100);

    vs.upsert(
        &[
            ChunkVector {
                chunk_id: id1,
                vector: vec![1.0, 0.0, 0.0, 0.0],
                file_path: "a.rs".into(),
                content: "a".repeat(1600),
            },
            ChunkVector {
                chunk_id: id2,
                vector: vec![0.9, 0.1, 0.0, 0.0],
                file_path: "b.rs".into(),
                content: "b".repeat(1600),
            },
            ChunkVector {
                chunk_id: id3,
                vector: vec![0.8, 0.2, 0.0, 0.0],
                file_path: "c.rs".into(),
                content: "c".repeat(400),
            },
        ],
        dim,
    )
    .unwrap();

    let retrieved = retrieve::search(&vs, &conn, &[1.0, 0.0, 0.0, 0.0], 3).unwrap();
    assert_eq!(retrieved.len(), 3);

    // Budget: 500 tokens → fits chunk1 (400) + chunk3 (100), skips chunk2 (400).
    let project_tokens = tokens::project_total(&conn).unwrap();
    let ctx = context::assemble(retrieved, 500, project_tokens);

    assert_eq!(ctx.context_tokens, 500);
    assert_eq!(ctx.dropped_count, 1);
    assert_eq!(ctx.chunks.len(), 2);
    assert!(ctx.theoretical_saved > 0);
    assert_eq!(ctx.real_cost(), 500);
}

#[test]
fn project_total_sums_all_chunk_token_estimates() {
    let (_tmp, conn, _vs) = setup();

    insert_chunk(&conn, "x.rs", 1, 5, 100);
    insert_chunk(&conn, "y.rs", 1, 5, 250);
    insert_chunk(&conn, "z.rs", 1, 5, 50);

    let total = tokens::project_total(&conn).unwrap();
    assert_eq!(total, 400);
}
