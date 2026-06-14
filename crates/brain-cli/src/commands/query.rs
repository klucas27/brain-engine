//! `brain query` — embed a question and return the most relevant code chunks.
//!
//! Pipeline:
//!  1. Embed the query text using the same model that built the index
//!  2. ANN top-k search in LanceDB
//!  3. Enrich hits with file/line metadata from SQLite
//!  4. Assemble chunks within the token budget
//!  5. Display results with file:line citations and a token report

use std::path::Path;

use brain_core::cache::{self, CacheLookup};
use brain_core::config::{self, GlobalConfig, ProjectConfig, Providers};
use brain_core::context::{self, Context};
use brain_core::db;
use brain_core::llm_state;
use brain_core::metrics::{self, RequestMetric};
use brain_core::model_router::{self, ModelDecision};
use brain_core::paths::{GlobalPaths, ProjectPaths};
use brain_core::retrieve;
use brain_core::router;
use brain_core::tokens;
use brain_core::vectors::VectorStore;
use brain_core::{BrainError, Result};

/// Execute `brain query` for `root`.
///
/// * `query_text`  — the natural-language question to find chunks for
/// * `top_k`       — number of ANN candidates to retrieve before budget trimming
/// * `budget`      — maximum context tokens to assemble
/// * `no_cache`    — skip the cache entirely (both read and write) when `true`
pub fn run(
    root: &Path,
    query_text: &str,
    top_k: usize,
    budget: usize,
    json: bool,
    no_cache: bool,
) -> Result<()> {
    let project = ProjectPaths::new(root.to_path_buf());

    if !project.is_initialised() {
        return Err(BrainError::Walk(format!(
            "no brain found at {} — run `brain init` first",
            project.brain_dir().display()
        )));
    }

    let cfg: ProjectConfig = config::load_or_default(&project.config_file())?;
    let global = GlobalPaths::resolve()?;
    let global_cfg: GlobalConfig = config::load_or_default(&global.config_file())?;
    let providers: Providers = config::load_or_default(&global.providers_file())?;

    let conn = db::open(&project.metadata_db())?;

    // Check that there is something to search.
    let total_chunks = tokens::chunk_count(&conn)?;
    if total_chunks == 0 {
        let msg = "no chunks indexed — run `brain index` first";
        if json {
            println!("{}", serde_json::json!({ "error": msg }));
        } else {
            eprintln!("brain: {msg}");
        }
        return Ok(());
    }

    let embedded_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM chunks WHERE vector_id IS NOT NULL",
        [],
        |r: &rusqlite::Row<'_>| r.get(0),
    )?;
    if embedded_count == 0 {
        let msg = "no embeddings found — run `brain index` (without --no-embed) first";
        if json {
            println!("{}", serde_json::json!({ "error": msg }));
        } else {
            eprintln!("brain: {msg}");
        }
        return Ok(());
    }

    // ── Embed the query ──────────────────────────────────────────────────────
    let models_dir = global.models_dir();
    std::fs::create_dir_all(&models_dir).map_err(|e| BrainError::io(&models_dir, e))?;

    let embedder =
        brain_embed::from_provider(&cfg.embedding_provider, &providers, Some(&models_dir))
            .map_err(|e| BrainError::Embed(e.to_string()))?;

    // Session id: prefer the Claude Code hook env var, fall back to PID-based.
    let session_id: Option<String> = std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .or_else(|| Some(format!("cli-{}", std::process::id())));
    if let Some(ref sid) = session_id {
        let _ = metrics::ensure_session(&conn, sid);
    }

    // Track total wall time from this point.
    let wall_start = std::time::Instant::now();

    // ── LLM routing ──────────────────────────────────────────────────
    // Check ~/.brain/llm_state.json for an active rate-limit block, then let
    // the router decide Claude vs DeepSeek. Non-fatal if the file is unreadable.
    let llm_state = llm_state::read(&global.llm_state_file()).unwrap_or_default();
    let is_claude_blocked = llm_state::is_blocked(&llm_state, "claude");
    let route_decision = router::route_live(&global_cfg.decision, None, is_claude_blocked);
    let llm_used = route_decision.route.as_str().to_string();

    // ── Model-tier routing (content-based, deterministic) ────────────
    // Classify the prompt and pick a model tier (opus/sonnet/…). Cheap and
    // local; surfaced via the `[MODEL ROUTER]` line. `None` when disabled.
    let model_decision: Option<ModelDecision> = if global_cfg.model_router.enabled {
        Some(model_router::route(query_text, &global_cfg.model_router))
    } else {
        None
    };

    // ── Cache lookup (before retrieval) ─────────────────────────────
    if !no_cache {
        // Cheap exact lookup before paying the embedding cost.
        let exact_hit = cache::lookup(
            &conn,
            &global_cfg.cache,
            query_text,
            embedder.model_id(),
            None, // no vector yet — exact check is free
        )
        .unwrap_or(CacheLookup::Miss);

        if let CacheLookup::Hit { response, kind } = exact_hit {
            // Record the cache-hit metric before returning.
            let _ = metrics::record_request(
                &conn,
                &project.logs_dir(),
                &RequestMetric {
                    session_id: session_id.clone(),
                    response_time_ms: Some(wall_start.elapsed().as_millis() as u64),
                    cache_hit: true,
                    ..Default::default()
                },
            );
            if json {
                let v = serde_json::json!({
                    "query": query_text,
                    "cache_hit": true,
                    "cache_kind": kind.as_str(),
                    "response": response,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("[cache:{kind}] {response}");
            }
            return Ok(());
        }
    }

    let query_start = std::time::Instant::now();
    let query_vecs = embedder
        .embed(&[query_text])
        .map_err(|e| BrainError::Embed(e.to_string()))?;
    let query_vector = &query_vecs[0];

    // ── Semantic cache lookup (after embedding) ─────────────────────
    if !no_cache && global_cfg.cache.semantic_enabled {
        let sem_hit = cache::lookup(
            &conn,
            &global_cfg.cache,
            query_text,
            embedder.model_id(),
            Some(query_vector),
        )
        .unwrap_or(CacheLookup::Miss);

        if let CacheLookup::Hit { response, kind } = sem_hit {
            let _ = metrics::record_request(
                &conn,
                &project.logs_dir(),
                &RequestMetric {
                    session_id: session_id.clone(),
                    response_time_ms: Some(wall_start.elapsed().as_millis() as u64),
                    cache_hit: true,
                    ..Default::default()
                },
            );
            if json {
                let v = serde_json::json!({
                    "query": query_text,
                    "cache_hit": true,
                    "cache_kind": kind.as_str(),
                    "response": response,
                });
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            } else {
                println!("[cache:{kind}] {response}");
            }
            return Ok(());
        }
    }

    // ── ANN search ──────────────────────────────────────────────────────────
    let vs = VectorStore::open(&project.vectors_dir())?;
    let retrieved = retrieve::search(&vs, &conn, query_vector, top_k)?;
    let retrieval_ms = query_start.elapsed().as_millis();

    // ── Project token total (for savings estimate) ──────────────────────────
    let project_tokens = tokens::project_total(&conn)?;

    // ── Context assembly ─────────────────────────────────────────────────────
    let ctx = context::assemble(retrieved, budget, project_tokens);

    // ── Store in cache ──────────────────────────────────────────────────
    if !no_cache && !ctx.chunks.is_empty() {
        // Build a compact text representation of the assembled context to cache.
        let cached_response = build_cache_response(&ctx);
        let store_vec = if global_cfg.cache.semantic_enabled {
            Some(query_vector.as_slice())
        } else {
            None
        };
        // Best-effort: cache failures must not break the query result.
        let _ = cache::store(
            &conn,
            &global_cfg.cache,
            query_text,
            embedder.model_id(),
            &cached_response,
            store_vec,
            None,
        );
        // Lazily remove expired entries after each write.
        let _ = cache::purge_expired(&conn);
    }

    // ── Record metrics ──────────────────────────────────────────────────
    let total_ms = wall_start.elapsed().as_millis() as u64;
    let metric = RequestMetric {
        session_id: session_id.clone(),
        response_time_ms: Some(total_ms),
        context_tokens_estimated: Some(ctx.context_tokens as i64),
        // Stored for reference only; never accumulated (see metrics.rs).
        tokens_saved_estimated: Some(ctx.theoretical_saved as i64),
        chunks_used: Some(ctx.chunks.len() as i64),
        retrieval_time_ms: Some(retrieval_ms as u64),
        embedding_source: Some("local".into()),
        llm_used: Some(llm_used),
        decision_reason: Some(route_decision.reason.clone()),
        cache_hit: false,
        ..Default::default()
    };
    let _ = metrics::record_request(&conn, &project.logs_dir(), &metric);

    if json {
        print_json(query_text, &ctx, retrieval_ms, embedder.model_id(), model_decision.as_ref());
    } else {
        print_human(query_text, &ctx, retrieval_ms, embedder.model_id(), model_decision.as_ref());
    }
    Ok(())
}

/// Render the `[MODEL ROUTER]` block for the human view.
fn print_model_router(d: &ModelDecision) {
    let c = &d.class;
    println!();
    println!("[MODEL ROUTER]");
    println!("  Type:           {}", c.req_type);
    println!("  Complexity:     {}", c.complexity);
    println!("  Critical:       {}", c.is_critical);
    println!("  Selected Model: {}", d.model.as_str().to_uppercase());
    println!("  Reason:         {}", d.reason);
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

const RULE: &str = "─────────────────────────────────────────────────────────────────────";

fn print_human(query: &str, ctx: &Context, ms: u128, model: &str, route: Option<&ModelDecision>) {
    println!("Query:  {query}");
    println!("Model:  {model}");
    println!();

    if ctx.chunks.is_empty() {
        println!("No relevant chunks found in the index.");
        return;
    }

    for (i, chunk) in ctx.chunks.iter().enumerate() {
        println!(
            "[{}] {} │ score {:.3} │ ~{} tokens",
            i + 1,
            retrieve::citation(chunk),
            chunk.score,
            chunk.token_estimate
        );
        println!("{RULE}");
        println!("{}", chunk.content.trim_end());
        println!();
    }

    println!("{RULE}");
    println!("[Brain Metrics]  ({ms}ms)");
    println!();
    println!("  Context injected:              {} tokens", ctx.context_tokens);
    println!("  Estimated project size:        {} tokens", ctx.project_tokens);
    println!("  Reduction (vs full project):   {:.1}%", ctx.reduction_pct());
    println!("  Real cost (added to prompt):   +{} tokens", ctx.real_cost());
    println!(
        "  Efficiency ratio:              {:.1}%",
        ctx.efficiency_ratio() * 100.0
    );
    if ctx.dropped_count > 0 {
        println!(
            "  dropped    {} chunk(s) exceeded the token budget",
            ctx.dropped_count
        );
    }

    if let Some(d) = route {
        print_model_router(d);
    }
}

fn print_json(query: &str, ctx: &Context, ms: u128, model: &str, route: Option<&ModelDecision>) {
    let chunks: Vec<serde_json::Value> = ctx
        .chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            serde_json::json!({
                "rank": i + 1,
                "file_path": c.file_path,
                "start_line": c.start_line,
                "end_line": c.end_line,
                "score": c.score,
                "token_estimate": c.token_estimate,
                "content": c.content,
            })
        })
        .collect();

    let model_router_json = route.map(|d| {
        serde_json::json!({
            "selected_model": d.model.as_str(),
            "classification": d.class,
            "scores":         d.scores,
            "reason":         d.reason,
        })
    });

    let value = serde_json::json!({
        "query": query,
        "model": model,
        "chunks": chunks,
        "model_router": model_router_json,
        "stats": {
            "retrieval_ms": ms,
            "context_tokens": ctx.context_tokens,
            "project_tokens_estimated": ctx.project_tokens,
            "real_cost": ctx.real_cost(),
            "theoretical_saved": ctx.theoretical_saved,
            "reduction_pct": ctx.reduction_pct(),
            "efficiency_ratio": ctx.efficiency_ratio(),
            "dropped_chunks": ctx.dropped_count,
        }
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}

// ---------------------------------------------------------------------------
// Cache helpers
// ---------------------------------------------------------------------------

/// Build a compact, human-readable representation of the assembled context
/// suitable for storing in the cache.  On a cache hit the CLI prints this
/// directly; it is not the full JSON output but carries all citations and
/// content needed to reproduce the human-readable view.
fn build_cache_response(ctx: &Context) -> String {
    let mut out = String::new();
    for chunk in &ctx.chunks {
        out.push_str(&retrieve::citation(chunk));
        out.push('\n');
        out.push_str(chunk.content.trim_end());
        out.push_str("\n\n");
    }
    out
}
