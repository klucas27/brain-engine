# Brain Engine — Architecture

> Local AI context layer for Claude Code. Reference document (kept ≤ 200 lines).
> Source of truth for *design*; `brain-plan.md` is the original spec.

## 1. Overview

Brain Engine is a **local intelligence layer** that sits between a project and
Claude Code. Instead of shipping the whole repository to the model on every
prompt, it indexes the code once, retrieves only the few most relevant chunks
per question, and injects them as context. Goals: fewer tokens, lower latency,
fewer hallucinations, zero friction, multi-project isolation, and a dynamic
choice between local and API processing.

Two brains coexist:

- **Global brain** `~/.brain/` — shared config, providers, cross-project cache,
  long-term memory, logs.
- **Project brain** `<root>/.brain/` — that project's index, embeddings,
  summaries, cache and metadata DB. Always git-ignored.

## 2. Main components

| Component | Tech | Responsibility |
|-----------|------|----------------|
| `brain-core` | Rust lib | Paths, config, SQLite schema + migrations, idempotent scaffolding. |
| `brain-cli` (`brain`) | Rust bin (clap) | User/automation entry point: `init`, `status`, later `index`, `query`, `stats`, `daemon`. |
| Indexer | Rust + Rayon | Parallel walk, secret/binary filtering, chunking, incremental reindex by content hash. |
| Embedder | Rust trait | Pluggable: local (bge-small via ONNX) or API (DeepSeek/OpenAI). One model pinned per index. |
| Vector store | LanceDB (Chroma fallback) | ANN search over chunk embeddings, under `.brain/vectors/`. |
| Metadata DB | SQLite (WAL) | Files, chunks, cache, sessions, request metrics. |
| Decision engine | Rust + sysinfo | Deterministic local-vs-API routing from live CPU/RAM + batch size + config thresholds. |
| AI router | Rust | Picks DeepSeek (cheap reads/embeddings) vs Claude (complex reasoning). |
| Cache | SQLite | Exact (SHA-256) cache with TTL; optional, opt-in semantic cache. |
| Daemon + watcher | Rust + notify | Long-lived process over a Unix socket; debounced incremental reindex. |
| Hooks | Shell + Node thin client | `UserPromptSubmit` injects retrieved context; `Stop` stores the response. |
| Metrics | SQLite + JSON logs | Per-request and per-session stats; `brain stats` consolidation. |

## 3. Data flow

### Indexing (background)
```
file change ──> watcher (debounce) ──> indexer (Rayon)
   ──> filter (.gitignore, size, secrets, binary)
   ──> chunk (line windows + overlap)
   ──> hash; skip unchanged
   ──> embedder (local|api per decision engine)
   ──> SQLite (files, chunks)  +  LanceDB (vectors)
```

### Retrieval (per prompt)
```
Claude Code prompt
   ──> UserPromptSubmit hook ──> Node client ──> daemon (Unix socket)
   ──> cache lookup (SHA-256) ── hit ─────────────────────────┐
                              └ miss                            │
   ──> embed query (decision engine: local|api)                │
   ──> LanceDB top-k ──> assemble context (token budget)       │
   ──> return context  <───────────────────────────────────────┘
   ──> hook injects context into the prompt
Claude responds ──> Stop hook ──> daemon stores response + metrics
```

Hard rule: **never send the whole project**; always retrieve, always try cache,
always try to reduce cost.

## 4. Filesystem layout

```
~/.brain/                      <root>/
  config.json                    brain.config.json
  providers.json                 .brain/
  cache/  memory/  logs/           vectors/   (LanceDB)
                                   cache/  summaries/  logs/
                                   metadata.db  (SQLite, WAL)
                                 .claude/hooks/  (Phase 8)
```

## 5. Metadata schema (SQLite, v1)

`meta(key,value)` · `files(path,hash,size,mtime,lang,chunk_count,indexed_at)` ·
`chunks(file_id,ordinal,start_line,end_line,content,content_hash,token_estimate,vector_id)` ·
`cache(key,response,created_at,expires_at)` · `sessions(id,started_at,ended_at)` ·
`requests(...response_time_ms, context_tokens_estimated, tokens_saved_estimated,
chunks_used, retrieval_time_ms, embedding_source, llm_used, cpu_usage_percent,
memory_usage_mb, decision_reason, cache_hit)`.

The full schema ships in Phase 1; later phases only populate it. Migrations are
forward-only, tracked via `PRAGMA user_version`.

## 6. Key technical decisions

1. **Rust core as a long-lived daemon, not per-prompt processes.** Spawning a
   process (and reloading models) on every prompt would erase the latency
   savings. A daemon over a Unix socket keeps embeddings/model warm. *(Phase 7)*
2. **Drop Python from the hot path.** Local embeddings run in Rust (ONNX
   runtime) so there is one core runtime. Node is only a thin hook client.
3. **One embedding model pinned per index.** Mixing dimensions (bge 384 vs
   OpenAI 1536) corrupts ANN search, so the model id is stored in `meta` and a
   mismatch forces a reindex rather than silently degrading.
4. **Security first: secrets never leave the machine.** Indexing honours
   `.gitignore` and excludes `.env`, keys and certs by default; sending chunks
   to an external API is gated by this filter. *(Phases 2–3)*
5. **Deterministic, configurable decision engine.** Thresholds live in
   `config.json`; routing logs an explicit `decision_reason`, so behaviour is
   reproducible and tunable instead of opaque heuristics.
6. **Semantic cache is opt-in.** Exact SHA-256 caching is always safe; fuzzy
   semantic hits can return a wrong answer, so they default to off behind a
   similarity threshold. *(Phase 6)*
7. **`.brain/` is git-ignored automatically.** It holds large, machine-local
   artifacts (vectors, SQLite, caches) that must never enter version control.
8. **Idempotent, non-destructive `init`.** Safe to run on every Claude Code
   launch; existing config and data are never overwritten.
9. **WAL + busy-timeout for multi-session.** Multiple Claude sessions on one
   repo read concurrently while the indexer writes, without lock failures.
10. **Provider abstraction over a trait + `providers.json`.** No hard dependency
    on DeepSeek; embedding and LLM providers are swappable per project.

## 7. Honest constraints / risks

- Claude Code hooks can *add* context (`UserPromptSubmit` stdout) but cannot
  arbitrarily rewrite the model's full input; injection is additive.
- `tokens_saved_estimated` is an estimate against a "whole-project" baseline —
  useful as a trend, not an exact figure.
- LanceDB/ONNX add native build weight; Chroma and API-only embeddings are the
  documented fallbacks for constrained environments.
- Decision-engine thresholds need real-world tuning; metrics (Phase 9) feed that
  loop.

## 8. Build & test

`cargo build` / `cargo test` at the workspace root. The `brain` binary is the
single entry point. See `IMPLEMENTATION_PLAN.md` for the phase breakdown and
`README.md` for usage.
