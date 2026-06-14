//! Blocking worker thread that owns all non-Send state.
//!
//! `rusqlite::Connection`, `VectorStore` (wraps a Tokio current-thread runtime),
//! and `Box<dyn Embedder>` all carry constraints that make sharing them across
//! async tasks awkward.  Instead the daemon runs a single dedicated OS thread
//! that owns all of this state for its entire lifetime.
//!
//! Callers communicate with the worker via a [`std::sync::mpsc::SyncSender`]
//! carrying [`WorkerMsg`] variants; each variant embeds a
//! [`tokio::sync::oneshot::Sender`] for the reply, which is `Send` and therefore
//! safe to pass from async land into the worker thread and back.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use brain_core::cache::{self, CacheLookup};
use brain_core::config::{self, GlobalConfig, ProjectConfig, Providers};
use brain_core::context;
use brain_core::db;
use brain_core::index;
use brain_core::metrics::{self, RequestMetric};
use brain_core::model_router;
use brain_core::paths::{GlobalPaths, ProjectPaths};
use brain_core::retrieve;
use brain_core::tokens;
use brain_core::vectors::{ChunkVector, VectorStore};
use brain_embed::Embedder;
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Message type
// ---------------------------------------------------------------------------

/// Messages that can be sent to the worker thread.
pub enum WorkerMsg {
    Ping {
        reply: oneshot::Sender<()>,
    },
    Status {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Query {
        query: String,
        top_k: usize,
        tokens: usize,
        no_cache: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Index {
        reindex: bool,
        no_embed: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Store {
        query: String,
        response: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Worker state (lives entirely on the worker thread)
// ---------------------------------------------------------------------------

struct WorkerState {
    project_root: PathBuf,
    project_paths: ProjectPaths,
    cfg: ProjectConfig,
    global_cfg: GlobalConfig,
    conn: Connection,
    vs: VectorStore,
    embedder: Box<dyn Embedder>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Spawn the worker thread and return a channel sender.
///
/// Blocks until the worker has fully initialized (models loaded, DB open) so
/// callers can detect startup failures synchronously.
pub fn spawn(root: PathBuf) -> Result<mpsc::SyncSender<WorkerMsg>, String> {
    let (tx, rx) = mpsc::sync_channel::<WorkerMsg>(64);

    // Init-result channel: worker notifies us once state is ready.
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

    std::thread::spawn(move || {
        match init_state(&root) {
            Ok(state) => {
                let _ = init_tx.send(Ok(()));
                run_loop(state, rx);
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
                // rx is dropped → sender side will get disconnected errors.
            }
        }
    });

    init_rx
        .recv()
        .map_err(|_| "worker thread exited before init".to_string())??;
    Ok(tx)
}

// ---------------------------------------------------------------------------
// Initialisation (runs on the worker thread)
// ---------------------------------------------------------------------------

fn init_state(root: &Path) -> Result<WorkerState, String> {
    let project_paths = ProjectPaths::new(root.to_path_buf());

    if !project_paths.is_initialised() {
        return Err(format!(
            "no brain found at {} — run `brain init` first",
            project_paths.brain_dir().display()
        ));
    }

    let cfg: ProjectConfig =
        config::load_or_default(&project_paths.config_file()).map_err(|e| e.to_string())?;

    let global_paths = GlobalPaths::resolve().map_err(|e| e.to_string())?;
    let global_cfg: GlobalConfig =
        config::load_or_default(&global_paths.config_file()).map_err(|e| e.to_string())?;
    let providers: Providers =
        config::load_or_default(&global_paths.providers_file()).map_err(|e| e.to_string())?;

    let models_dir = global_paths.models_dir();
    std::fs::create_dir_all(&models_dir).map_err(|e| format!("create models dir: {e}"))?;

    let embedder =
        brain_embed::from_provider(&cfg.embedding_provider, &providers, Some(&models_dir))
            .map_err(|e| e.to_string())?;

    let conn = db::open(&project_paths.metadata_db()).map_err(|e| e.to_string())?;
    let vs = VectorStore::open(&project_paths.vectors_dir()).map_err(|e| e.to_string())?;

    // Clean up expired cache entries on startup.
    let _ = cache::purge_expired(&conn);

    Ok(WorkerState {
        project_root: root.to_path_buf(),
        project_paths,
        cfg,
        global_cfg,
        conn,
        vs,
        embedder,
    })
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

fn run_loop(mut state: WorkerState, rx: mpsc::Receiver<WorkerMsg>) {
    for msg in rx {
        match msg {
            WorkerMsg::Ping { reply } => {
                let _ = reply.send(());
            }
            WorkerMsg::Shutdown => break,
            WorkerMsg::Status { reply } => {
                let _ = reply.send(handle_status(&state));
            }
            WorkerMsg::Query {
                query,
                top_k,
                tokens,
                no_cache,
                reply,
            } => {
                let _ = reply.send(handle_query(&state, &query, top_k, tokens, no_cache));
            }
            WorkerMsg::Index {
                reindex,
                no_embed,
                reply,
            } => {
                let _ = reply.send(handle_index(&mut state, reindex, no_embed));
            }
            WorkerMsg::Store {
                query,
                response,
                reply,
            } => {
                let _ = reply.send(handle_store(&state, &query, &response));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_status(s: &WorkerState) -> Result<Value, String> {
    let (files, chunks) = db::counts(&s.conn).map_err(|e| e.to_string())?;
    let vectors = s.vs.count().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "project":         s.cfg.project_name,
        "root":            s.project_root.display().to_string(),
        "files":           files,
        "chunks":          chunks,
        "vectors":         vectors,
        "embedding_model": s.embedder.model_id(),
    }))
}

fn handle_query(
    s: &WorkerState,
    query_text: &str,
    top_k: usize,
    tokens: usize,
    no_cache: bool,
) -> Result<Value, String> {
    let wall_start = std::time::Instant::now();

    // Generate a stable daemon session id (reused for all queries in this daemon lifetime).
    let session_id = format!("daemon-{}", std::process::id());
    let _ = metrics::ensure_session(&s.conn, &session_id);

    // Content-based model routing (cheap, deterministic). Computed once and
    // attached to every response shape (cache hit and live retrieval) so the
    // client can always render the `[MODEL ROUTER]` line. Honors the toggle.
    let model_router_json = build_model_router_json(&s.global_cfg.model_router, query_text);

    // Exact cache lookup before paying the embedding cost.
    if !no_cache {
        let hit = cache::lookup(
            &s.conn,
            &s.global_cfg.cache,
            query_text,
            s.embedder.model_id(),
            None,
        )
        .unwrap_or(CacheLookup::Miss);
        if let CacheLookup::Hit { response, kind } = hit {
            let _ = metrics::record_request(
                &s.conn,
                &s.project_paths.logs_dir(),
                &RequestMetric {
                    session_id: Some(session_id.clone()),
                    response_time_ms: Some(wall_start.elapsed().as_millis() as u64),
                    cache_hit: true,
                    ..Default::default()
                },
            );
            return Ok(serde_json::json!({
                "cache_hit":    true,
                "cache_kind":   kind.as_str(),
                "response":     response,
                "chunks":       [],
                "model_router": model_router_json,
            }));
        }
    }

    let vecs = s.embedder.embed(&[query_text]).map_err(|e| e.to_string())?;
    let qv = &vecs[0];

    // Semantic cache lookup.
    if !no_cache && s.global_cfg.cache.semantic_enabled {
        let hit = cache::lookup(
            &s.conn,
            &s.global_cfg.cache,
            query_text,
            s.embedder.model_id(),
            Some(qv),
        )
        .unwrap_or(CacheLookup::Miss);
        if let CacheLookup::Hit { response, kind } = hit {
            let _ = metrics::record_request(
                &s.conn,
                &s.project_paths.logs_dir(),
                &RequestMetric {
                    session_id: Some(session_id.clone()),
                    response_time_ms: Some(wall_start.elapsed().as_millis() as u64),
                    cache_hit: true,
                    ..Default::default()
                },
            );
            return Ok(serde_json::json!({
                "cache_hit":    true,
                "cache_kind":   kind.as_str(),
                "response":     response,
                "chunks":       [],
                "model_router": model_router_json,
            }));
        }
    }

    let retrieval_start = std::time::Instant::now();
    let retrieved = retrieve::search(&s.vs, &s.conn, qv, top_k).map_err(|e| e.to_string())?;
    let retrieval_ms = retrieval_start.elapsed().as_millis() as u64;
    let project_tokens = tokens::project_total(&s.conn).map_err(|e| e.to_string())?;
    let ctx = context::assemble(retrieved, tokens, project_tokens);

    // Store result in cache.
    if !no_cache && !ctx.chunks.is_empty() {
        let cached = ctx
            .chunks
            .iter()
            .map(|c| {
                format!(
                    "{}:{}-{}\n{}\n",
                    c.file_path,
                    c.start_line,
                    c.end_line,
                    c.content.trim_end()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let sv = s.global_cfg.cache.semantic_enabled.then_some(qv.as_slice());
        let _ = cache::store(
            &s.conn,
            &s.global_cfg.cache,
            query_text,
            s.embedder.model_id(),
            &cached,
            sv,
            None,
        );
        let _ = cache::purge_expired(&s.conn);
    }

    // Record metrics for the live retrieval.
    let _ = metrics::record_request(
        &s.conn,
        &s.project_paths.logs_dir(),
        &RequestMetric {
            session_id: Some(session_id),
            response_time_ms: Some(wall_start.elapsed().as_millis() as u64),
            context_tokens_estimated: Some(ctx.context_tokens as i64),
            // Stored for reference only; never accumulated (see metrics.rs).
            tokens_saved_estimated: Some(ctx.theoretical_saved as i64),
            chunks_used: Some(ctx.chunks.len() as i64),
            retrieval_time_ms: Some(retrieval_ms),
            embedding_source: Some("local".into()),
            cache_hit: false,
            ..Default::default()
        },
    );

    let chunks_json: Vec<Value> = ctx
        .chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            serde_json::json!({
                "rank":           i + 1,
                "file_path":      c.file_path,
                "start_line":     c.start_line,
                "end_line":       c.end_line,
                "score":          c.score,
                "token_estimate": c.token_estimate,
                "content":        c.content,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "cache_hit": false,
        "chunks": chunks_json,
        "model_router": model_router_json,
        "stats": {
            "context_tokens":    ctx.context_tokens,
            "project_tokens":    ctx.project_tokens,
            "real_cost":         ctx.real_cost(),
            "theoretical_saved": ctx.theoretical_saved,
            "reduction_pct":     ctx.reduction_pct(),
            "efficiency_ratio":  ctx.efficiency_ratio(),
            "dropped_chunks":    ctx.dropped_count,
        }
    }))
}

/// Build the `model_router` JSON object for a query response, or `null` when
/// the router is disabled in config. Pure and cheap — safe on the hot path.
fn build_model_router_json(cfg: &config::ModelRouterConfig, prompt: &str) -> Value {
    if !cfg.enabled {
        return Value::Null;
    }
    let d = model_router::route(prompt, cfg);
    serde_json::json!({
        "selected_model": d.model.as_str(),
        "classification": d.class,
        "scores":         d.scores,
        "reason":         d.reason,
    })
}

fn handle_index(s: &mut WorkerState, reindex: bool, no_embed: bool) -> Result<Value, String> {
    // Reload config in case it changed since daemon started.
    s.cfg = config::load_or_default(&s.project_paths.config_file()).map_err(|e| e.to_string())?;

    let stats =
        index::index_project(&s.project_root, &s.cfg, &mut s.conn).map_err(|e| e.to_string())?;

    if no_embed {
        return Ok(serde_json::json!({
            "scanned":        stats.scanned,
            "indexed":        stats.indexed,
            "unchanged":      stats.unchanged,
            "removed":        stats.removed,
            "chunks_written": stats.chunks_written,
            "embedded":       0,
        }));
    }

    if reindex {
        s.conn
            .execute("UPDATE chunks SET vector_id = NULL", [])
            .map_err(|e| e.to_string())?;
        s.vs.clear().map_err(|e| e.to_string())?;
        db::set_meta(&s.conn, "embedding.model_id", s.embedder.model_id())
            .map_err(|e| e.to_string())?;
        db::set_meta(&s.conn, "embedding.dim", &s.embedder.dim().to_string())
            .map_err(|e| e.to_string())?;
    }

    // Prune orphaned vectors.
    let all_ids = chunk_ids(&s.conn)?;
    s.vs.delete_orphans(&all_ids).map_err(|e| e.to_string())?;

    let pending = pending_chunks(&s.conn)?;
    let mut embedded = 0usize;
    let dim = s.embedder.dim();

    for batch in pending.chunks(64) {
        let texts: Vec<&str> = batch.iter().map(|c| c.content.as_str()).collect();
        let vecs = s.embedder.embed(&texts).map_err(|e| e.to_string())?;

        let cvs: Vec<ChunkVector> = batch
            .iter()
            .zip(vecs.iter())
            .map(|(c, v)| ChunkVector {
                chunk_id: c.id,
                vector: v.clone(),
                file_path: c.file_path.clone(),
                content: c.content.clone(),
            })
            .collect();

        s.vs.upsert(&cvs, dim).map_err(|e| e.to_string())?;

        for c in batch {
            s.conn
                .execute(
                    "UPDATE chunks SET vector_id = ?1 WHERE id = ?2",
                    rusqlite::params![c.id.to_string(), c.id],
                )
                .map_err(|e| e.to_string())?;
        }
        embedded += batch.len();
    }

    Ok(serde_json::json!({
        "scanned":        stats.scanned,
        "indexed":        stats.indexed,
        "unchanged":      stats.unchanged,
        "removed":        stats.removed,
        "chunks_written": stats.chunks_written,
        "embedded":       embedded,
    }))
}

fn handle_store(s: &WorkerState, query: &str, response: &str) -> Result<Value, String> {
    let qv = if s.global_cfg.cache.semantic_enabled {
        let vecs = s.embedder.embed(&[query]).map_err(|e| e.to_string())?;
        Some(vecs[0].clone())
    } else {
        None
    };
    cache::store(
        &s.conn,
        &s.global_cfg.cache,
        query,
        s.embedder.model_id(),
        response,
        qv.as_deref(),
        None,
    )
    .map_err(|e| e.to_string())?;
    let _ = cache::purge_expired(&s.conn);
    Ok(serde_json::json!({ "stored": true }))
}

// ---------------------------------------------------------------------------
// DB helpers local to the worker
// ---------------------------------------------------------------------------

struct PendingChunk {
    id: i64,
    content: String,
    file_path: String,
}

fn pending_chunks(conn: &Connection) -> Result<Vec<PendingChunk>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.content, f.path FROM chunks c \
             JOIN files f ON c.file_id = f.id WHERE c.vector_id IS NULL",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingChunk {
                id: r.get(0)?,
                content: r.get(1)?,
                file_path: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

fn chunk_ids(conn: &Connection) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare("SELECT id FROM chunks")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.map_err(|e| e.to_string())?);
    }
    Ok(ids)
}
