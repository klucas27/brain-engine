//! Retrieval layer: ANN search in LanceDB + metadata enrichment from SQLite.
//!
//! The core operation is:
//! 1. Vector-search LanceDB for the `top_k` chunks closest to `query_vector`
//! 2. Enrich each hit with `start_line`, `end_line`, and `token_estimate` from
//!    SQLite (which LanceDB doesn't store)
//! 3. Return a ranked `Vec<RetrievedChunk>` sorted by descending similarity

use rusqlite::Connection;

use crate::error::Result;
use crate::tokens;
use crate::vectors::{SearchHit, VectorStore};

/// A fully enriched chunk returned by the retrieval pipeline.
#[derive(Debug, Clone)]
pub struct RetrievedChunk {
    /// SQLite primary key (also serves as the LanceDB `chunk_id`).
    pub chunk_id: i64,
    /// Project-relative path of the source file.
    pub file_path: String,
    /// First source line included in this chunk (1-based, inclusive).
    pub start_line: usize,
    /// Last source line included in this chunk (1-based, inclusive).
    pub end_line: usize,
    /// Raw chunk text.
    pub content: String,
    /// Cosine similarity score in [0, 1]; higher = more relevant.
    pub score: f32,
    /// Estimated token count (bytes / 4).
    pub token_estimate: usize,
}

/// Search for the `top_k` chunks most similar to `query_vector`.
///
/// Returns an empty list when no chunks are embedded yet (the vector store is
/// empty or hasn't been created).  Results are sorted by descending score.
pub fn search(
    vs: &VectorStore,
    conn: &Connection,
    query_vector: &[f32],
    top_k: usize,
) -> Result<Vec<RetrievedChunk>> {
    let hits = vs.search(query_vector, top_k)?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    enrich(conn, hits)
}

/// Join LanceDB search hits with SQLite metadata.
fn enrich(conn: &Connection, hits: Vec<SearchHit>) -> Result<Vec<RetrievedChunk>> {
    if hits.is_empty() {
        return Ok(Vec::new());
    }

    // Build a lookup by chunk_id from SQLite in one round-trip.
    let ids_str: String = hits
        .iter()
        .map(|h| h.chunk_id.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT id, start_line, end_line, token_estimate \
         FROM chunks WHERE id IN ({ids_str})"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r: &rusqlite::Row<'_>| {
        Ok((
            r.get::<_, i64>(0)?, // id
            r.get::<_, i64>(1)?, // start_line
            r.get::<_, i64>(2)?, // end_line
            r.get::<_, i64>(3)?, // token_estimate
        ))
    })?;

    let mut meta: std::collections::HashMap<i64, (usize, usize, usize)> =
        std::collections::HashMap::new();
    for row in rows {
        let (id, sl, el, te) = row?;
        meta.insert(id, (sl as usize, el as usize, te as usize));
    }

    // Assemble enriched results, preserving the ANN ranking order.
    let mut results = Vec::with_capacity(hits.len());
    for hit in hits {
        let (start_line, end_line, mut token_estimate) =
            meta.get(&hit.chunk_id).copied().unwrap_or((0, 0, 0));

        // Fall back to the byte-heuristic if SQLite has no stored estimate.
        if token_estimate == 0 {
            token_estimate = tokens::estimate(&hit.content);
        }

        results.push(RetrievedChunk {
            chunk_id: hit.chunk_id,
            file_path: hit.file_path,
            start_line,
            end_line,
            content: hit.content,
            score: hit.score,
            token_estimate,
        });
    }

    Ok(results)
}

/// Format a citation string for display: `src/foo.rs:10-45`.
pub fn citation(chunk: &RetrievedChunk) -> String {
    format!(
        "{}:{}-{}",
        chunk.file_path, chunk.start_line, chunk.end_line
    )
}
