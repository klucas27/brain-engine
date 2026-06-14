//! `brain index` — scan the project, rebuild the chunk index, embed chunks.
//!
//! Execution order (all incremental):
//! 1. Walk + chunk + write to SQLite (`brain-core::index`)
//! 2. Detect embedding model changes via the `meta` table
//! 3. Prune orphaned LanceDB entries (chunks deleted in step 1)
//! 4. Embed any chunk with `vector_id IS NULL` (new or changed chunks)
//! 5. Update `chunks.vector_id` in SQLite for each newly embedded chunk

use std::path::Path;

use brain_core::config::{self, ProjectConfig, Providers};
use brain_core::db;
use brain_core::index::{self, IndexStats};
use brain_core::paths::{GlobalPaths, ProjectPaths};
use brain_core::vectors::{ChunkVector, VectorStore};
use brain_core::{BrainError, Result};
use brain_embed::Embedder;
use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Execute `brain index` for `root`.
///
/// * `reindex`  — force clearing and re-embedding all chunks
/// * `no_embed` — skip the embedding step entirely (chunk index only)
pub fn run(root: &Path, json: bool, reindex: bool, no_embed: bool) -> Result<()> {
    let project = ProjectPaths::new(root.to_path_buf());

    if !project.is_initialised() {
        return Err(BrainError::Walk(format!(
            "no brain found at {} — run `brain init` first",
            project.brain_dir().display()
        )));
    }

    let cfg: ProjectConfig = config::load_or_default(&project.config_file())?;

    let mut conn = db::open(&project.metadata_db())?;

    // ── Step 1: incremental chunk indexing ──────────────────────────────────
    let index_start = std::time::Instant::now();
    let index_stats = index::index_project(root, &cfg, &mut conn)?;
    let index_ms = index_start.elapsed().as_millis();

    if no_embed {
        // Fast path: skip all embedding work; output flat JSON for CLI compat.
        if json {
            print_json_index_only(&index_stats, index_ms);
        } else {
            print_human_index_only(&index_stats, index_ms);
        }
        return Ok(());
    }

    // ── Step 2: resolve embedder ─────────────────────────────────────────────
    let global = GlobalPaths::resolve()?;
    let providers: Providers = config::load_or_default(&global.providers_file())?;

    let models_dir = global.models_dir();
    std::fs::create_dir_all(&models_dir).map_err(|e| BrainError::io(&models_dir, e))?;

    let embedder =
        brain_embed::from_provider(&cfg.embedding_provider, &providers, Some(&models_dir))
            .map_err(|e| BrainError::Embed(e.to_string()))?;

    // ── Step 3: model-pin check ─────────────────────────────────────────────
    let need_full_reindex = reindex || {
        match check_model_pin(&conn, embedder.as_ref())? {
            ModelPinStatus::Mismatch {
                ref pinned_model,
                ref current_model,
                ..
            } => {
                eprintln!(
                    "⚠  Embedding model changed ({pinned_model} → {current_model}); \
                     rebuilding all vectors."
                );
                true
            }
            _ => false,
        }
    };

    let vs = VectorStore::open(&project.vectors_dir())?;

    if need_full_reindex {
        conn.execute("UPDATE chunks SET vector_id = NULL", [])?;
        db::set_meta(&conn, "embedding.model_id", embedder.model_id())?;
        db::set_meta(&conn, "embedding.dim", &embedder.dim().to_string())?;
        vs.clear()?;
    } else {
        set_model_pin_if_absent(&conn, embedder.as_ref())?;
    }

    // ── Step 4: prune orphaned LanceDB entries ────────────────────────────
    let all_sqlite_ids = get_all_chunk_ids(&conn)?;
    vs.delete_orphans(&all_sqlite_ids)?;

    // ── Step 5: embed pending chunks ──────────────────────────────────────
    let embed_start = std::time::Instant::now();
    let embed_stats = embed_pending_chunks(&conn, embedder.as_ref(), &vs)?;
    let embed_ms = embed_start.elapsed().as_millis();

    let total_vectors = vs.count()?;

    if json {
        print_json(
            &index_stats,
            index_ms,
            &embed_stats,
            embed_ms,
            total_vectors,
        );
    } else {
        print_human(
            &index_stats,
            index_ms,
            &embed_stats,
            embed_ms,
            total_vectors,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Model-pin helpers
// ---------------------------------------------------------------------------

enum ModelPinStatus {
    /// No pin recorded yet — first index+embed run.
    Absent,
    /// Pinned model matches the current embedder.
    Match,
    /// Pinned model differs — caller must decide whether to abort or reindex.
    Mismatch {
        pinned_model: String,
        #[allow(dead_code)]
        pinned_dim: usize,
        current_model: String,
        #[allow(dead_code)]
        current_dim: usize,
    },
}

/// Read the pinned model from `meta` and compare with the current embedder.
fn check_model_pin(conn: &Connection, embedder: &dyn Embedder) -> Result<ModelPinStatus> {
    let pinned_model = db::get_meta(conn, "embedding.model_id")?;
    let pinned_dim: Option<usize> =
        db::get_meta(conn, "embedding.dim")?.and_then(|s| s.parse().ok());

    match (pinned_model, pinned_dim) {
        (None, _) | (_, None) => Ok(ModelPinStatus::Absent),
        (Some(pm), Some(pd)) if pm == embedder.model_id() && pd == embedder.dim() => {
            Ok(ModelPinStatus::Match)
        }
        (Some(pm), Some(pd)) => Ok(ModelPinStatus::Mismatch {
            pinned_model: pm,
            pinned_dim: pd,
            current_model: embedder.model_id().to_string(),
            current_dim: embedder.dim(),
        }),
    }
}

/// Write the model pin to `meta` only when no pin exists yet.
fn set_model_pin_if_absent(conn: &Connection, embedder: &dyn Embedder) -> Result<()> {
    if db::get_meta(conn, "embedding.model_id")?.is_none() {
        db::set_meta(conn, "embedding.model_id", embedder.model_id())?;
        db::set_meta(conn, "embedding.dim", &embedder.dim().to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Embedding orchestration
// ---------------------------------------------------------------------------

struct PendingChunk {
    id: i64,
    content: String,
    file_path: String,
}

struct EmbedStats {
    embedded: usize,
    already_embedded: usize,
}

fn get_all_chunk_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM chunks")?;
    let rows = stmt.query_map([], |r: &rusqlite::Row<'_>| r.get::<_, i64>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

fn get_pending_chunks(conn: &Connection) -> Result<Vec<PendingChunk>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.content, f.path
         FROM chunks c
         JOIN files f ON c.file_id = f.id
         WHERE c.vector_id IS NULL",
    )?;
    let rows = stmt.query_map([], |r: &rusqlite::Row<'_>| {
        Ok(PendingChunk {
            id: r.get(0)?,
            content: r.get(1)?,
            file_path: r.get(2)?,
        })
    })?;
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row?);
    }
    Ok(pending)
}

fn already_embedded_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunks WHERE vector_id IS NOT NULL",
        [],
        |r: &rusqlite::Row<'_>| r.get::<_, i64>(0),
    )?;
    Ok(n as usize)
}

/// Embed all chunks with `vector_id IS NULL`, write to LanceDB, then mark
/// them in SQLite.  Processes in batches of 64 for memory efficiency.
fn embed_pending_chunks(
    conn: &Connection,
    embedder: &dyn Embedder,
    vs: &VectorStore,
) -> Result<EmbedStats> {
    let already_embedded = already_embedded_count(conn)?;
    let pending = get_pending_chunks(conn)?;

    if pending.is_empty() {
        return Ok(EmbedStats {
            embedded: 0,
            already_embedded,
        });
    }

    let dim = embedder.dim();
    const BATCH: usize = 64;
    let mut embedded = 0usize;

    for batch in pending.chunks(BATCH) {
        let texts: Vec<&str> = batch.iter().map(|c| c.content.as_str()).collect();

        let vectors = embedder
            .embed(&texts)
            .map_err(|e| BrainError::Embed(e.to_string()))?;

        let chunk_vectors: Vec<ChunkVector> = batch
            .iter()
            .zip(vectors.iter())
            .map(|(c, v)| ChunkVector {
                chunk_id: c.id,
                vector: v.clone(),
                file_path: c.file_path.clone(),
                content: c.content.clone(),
            })
            .collect();

        vs.upsert(&chunk_vectors, dim)?;

        // Mark each chunk as embedded — we use the chunk's own SQLite id as
        // the vector_id since it is the join key between the two stores.
        for c in batch {
            conn.execute(
                "UPDATE chunks SET vector_id = ?1 WHERE id = ?2",
                rusqlite::params![c.id.to_string(), c.id],
            )?;
        }

        embedded += batch.len();
    }

    Ok(EmbedStats {
        embedded,
        already_embedded,
    })
}

// ---------------------------------------------------------------------------
// Output formatters
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Human-readable output
// ---------------------------------------------------------------------------

fn print_human_index_only(s: &IndexStats, index_ms: u128) {
    println!("✓ Indexed in {index_ms}ms");
    print_index_fields(s);
}

fn print_human(
    s: &IndexStats,
    index_ms: u128,
    e: &EmbedStats,
    embed_ms: u128,
    total_vectors: usize,
) {
    println!("✓ Indexed in {index_ms}ms");
    print_index_fields(s);
    println!();
    println!("✓ Embedded in {embed_ms}ms");
    println!("  embedded   {}", e.embedded);
    println!("  cached     {}", e.already_embedded);
    println!("  vectors    {total_vectors}  (total in store)");
}

fn print_index_fields(s: &IndexStats) {
    println!("  scanned    {}", s.scanned);
    println!("  indexed    {}", s.indexed);
    println!("  unchanged  {}", s.unchanged);
    println!("  removed    {}", s.removed);
    println!("  chunks     {}", s.chunks_written);
    if s.skipped_binary + s.skipped_large + s.skipped_excluded > 0 {
        println!(
            "  skipped    {} binary, {} large, {} excluded",
            s.skipped_binary, s.skipped_large, s.skipped_excluded
        );
    }
}

// ---------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------

/// Flat JSON output (Phase 2 compat), used when `--no-embed` is passed.
fn print_json_index_only(s: &IndexStats, index_ms: u128) {
    let value = serde_json::json!({
        "elapsed_ms": index_ms,
        "scanned": s.scanned,
        "indexed": s.indexed,
        "unchanged": s.unchanged,
        "removed": s.removed,
        "chunks_written": s.chunks_written,
        "skipped_binary": s.skipped_binary,
        "skipped_large": s.skipped_large,
        "skipped_excluded": s.skipped_excluded,
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

/// Nested JSON output including embed stats.
fn print_json(
    s: &IndexStats,
    index_ms: u128,
    e: &EmbedStats,
    embed_ms: u128,
    total_vectors: usize,
) {
    let value = serde_json::json!({
        "index": {
            "elapsed_ms": index_ms,
            "scanned": s.scanned,
            "indexed": s.indexed,
            "unchanged": s.unchanged,
            "removed": s.removed,
            "chunks_written": s.chunks_written,
            "skipped_binary": s.skipped_binary,
            "skipped_large": s.skipped_large,
            "skipped_excluded": s.skipped_excluded,
        },
        "embed": {
            "elapsed_ms": embed_ms,
            "embedded": e.embedded,
            "already_embedded": e.already_embedded,
            "total_vectors": total_vectors,
        }
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
