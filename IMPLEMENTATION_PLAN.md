# Brain Engine — Incremental Implementation Plan

Each phase is **independent, testable, and small enough to ship on its own**.
A phase is "done" only when its completion criteria pass (tests + manual check).
Phases 1–10 are **fully implemented and shipped**.

Legend: 🟢 done.

---

## Phase 1 — Foundation & scaffolding 🟢

- **Objective:** Rust workspace, config model, SQLite metadata schema, and an
  idempotent `brain init` / `brain status`. Establishes the layout every later
  phase depends on.
- **Files:** `Cargo.toml`; `crates/brain-core/{Cargo.toml, src/{lib,error,paths,
  config,db,scaffold}.rs, tests/scaffold.rs}`;
  `crates/brain-cli/{Cargo.toml, src/{main.rs, commands/{mod,init,status}.rs},
  tests/cli.rs}`; `ARCHITECTURE.md`, `README.md`.
- **Tech:** Rust, clap, serde/serde_json, rusqlite (bundled, WAL), dirs.
- **Done when:** `cargo test` green; `brain init` creates both brains
  idempotently and git-ignores `.brain/`; `brain status` reports schema v1 and
  zero files/chunks. ✅

---

## Phase 2 — Indexer & chunking 🟢

- **Objective:** Walk the project, filter (gitignore/size/binary/secrets), chunk
  source into overlapping line windows, store in `files`/`chunks`, reindex
  incrementally by content hash. Parallel via Rayon.
- **Files:** `crates/brain-core/src/{walk.rs, chunk.rs, hash.rs, index.rs}`;
  `crates/brain-cli/src/commands/index.rs`; tests in `brain-core/tests/index.rs`.
- **Tech:** Rust, `ignore` (gitignore-aware walk), `globset`, `rayon`, `sha2`.
  Binary detection uses a NUL-byte heuristic (git's approach) instead of `infer`
  — more reliable for "is this chunkable text?" and dependency-free.
- **Done when:** `brain index` populates the DB; re-running with no changes does
  ~zero work; editing one file reindexes only that file; secrets/binaries are
  skipped (asserted in tests). ✅

---

## Phase 3 — Embeddings & vector store 🟢

- **Objective:** `Embedder` trait with a local backend (bge-small via ONNX) and
  the provider abstraction; write vectors to LanceDB; pin the model id in `meta`
  and refuse mismatches.
- **Files:** `crates/brain-embed/` (new crate: `embedder.rs`, `local.rs`,
  `provider.rs`); `crates/brain-core/src/vectors.rs` (LanceDB wrapper).
- **Tech:** Rust, `fastembed`/ONNX runtime, LanceDB (Chroma fallback documented).
- **Done when:** indexed chunks get embeddings of the pinned dimension; a
  model/dimension change triggers a guarded full reindex; vector count == chunk
  count. ✅

---

## Phase 4 — Retrieval & query 🟢

- **Objective:** Embed a question, ANN top-k over LanceDB, assemble context
  within a token budget, estimate tokens used/saved. `brain query "..."`.
- **Files:** `crates/brain-core/src/{retrieve.rs, context.rs, tokens.rs}`;
  `crates/brain-cli/src/commands/query.rs`.
- **Tech:** Rust, LanceDB search, simple token estimator.
- **Done when:** `brain query` returns the most relevant chunks with file/line
  citations and a token report; never returns the whole project. ✅

---

## Phase 5 — Decision engine & AI router 🟢

- **Objective:** Sample live CPU/RAM, apply deterministic config thresholds +
  batch size to choose local vs API embeddings, and route DeepSeek vs Claude.
  Always emit a `decision_reason`.
- **Files:** `crates/brain-core/src/{decision.rs, router.rs}`.
- **Tech:** Rust, `sysinfo`.
- **Done when:** unit tests cover each rule (high CPU→api, large batch→local,
  etc.); the chosen path and reason are logged and reproducible for fixed inputs. ✅

---

## Phase 6 — Cache layer 🟢

- **Objective:** Exact SHA-256 response cache with TTL; optional, opt-in semantic
  cache behind a similarity threshold.
- **Files:** `crates/brain-core/src/cache.rs`; wired into retrieval/query;
  `crates/brain-cli/src/commands/cache.rs` (`brain cache stats/clear/purge`).
- **Tech:** Rust, SQLite `cache` table (schema v2 adds `query_vector` column),
  cosine similarity.
- **Done when:** repeated identical queries hit the cache; expired entries miss;
  semantic cache stays off unless enabled and above threshold. ✅

---

## Phase 7 — Daemon & watcher 🟢

- **Objective:** Long-lived daemon over a Unix socket keeping models/index warm;
  file watcher with debounce drives incremental reindex; Node thin client.
- **Files:** `crates/brain-daemon/{Cargo.toml, src/{main,protocol,worker,watcher}.rs}`;
  `crates/brain-cli/src/commands/daemon.rs`; `clients/node/brain-client.mjs`.
- **Tech:** Rust, `notify` (v6, inotify on Linux), Unix domain socket at
  `.brain/brain.sock`, JSON-line protocol; tokio multi-thread accept loop;
  blocking worker thread owns `Connection`/`VectorStore`/`Embedder`; 500 ms
  debounce via std thread; Node ESM client.
- **Architecture:** Single worker thread owns all non-Send state; tokio accept
  loop communicates via `std::sync::mpsc::SyncSender<WorkerMsg>` +
  `tokio::sync::oneshot` per request. PID written to `.brain/brain.pid`.
- **CLI:** `brain daemon start|stop|status|query` (connects to socket).
- **Done when:** `brain daemon` serves query/index requests; saving a file
  reindexes it within the debounce window; multiple sessions share one daemon. ✅

---

## Phase 8 — Claude Code hooks & auto-bootstrap 🟢

- **Objective:** Transparent integration: `UserPromptSubmit` retrieves+injects
  context, `Stop` stores the response; auto-run `init` + daemon on first launch.
- **Files:** `.claude/hooks/{pre_prompt.sh, post_response.sh}` (generated by
  `brain install-hooks`); `crates/brain-cli/src/commands/install_hooks.rs`;
  `crates/brain-daemon/src/{protocol,worker,main}.rs` (new `store` method);
  `clients/node/brain-client.mjs` (new `store`, `hookPrompt`, `hookStop`).
- **Tech:** Shell hooks, Node client, Claude Code hook API (additive context).
- **Done when:** using Claude Code normally injects retrieved context with no
  manual commands; responses are stored; honest about additive-only injection. ✅

---

## Phase 9 — Metrics & logging 🟢

- **Objective:** Per-request + per-session metrics, JSON daily logs
  (`.brain/logs/YYYY-MM-DD.log`), the `[Brain Metrics]` CLI readout, and
  `brain stats` consolidation.
- **Files:** `crates/brain-core/src/metrics.rs` (new); `crates/brain-cli/src/commands/
  stats.rs` (new); `brain-cli/src/main.rs`, `commands/mod.rs`, `commands/query.rs`
  (modified); `brain-daemon/src/worker.rs` (modified); `brain-core/src/lib.rs`
  (modified).
- **Tech:** Rust, SQLite `requests`/`sessions` (schema already in place from
  Phase 1), JSON log lines (pure-Rust ISO-8601 formatter, no extra deps).
- **Architecture notes:**
  - `record_request` auto-calls `ensure_session` so callers have a single-call
    API and the FK constraint is never violated.
  - Both the CLI query path and the daemon worker record metrics (cache hits and
    live retrievals separately so hit rate is correct).
  - Session ID comes from `$CLAUDE_SESSION_ID` (Claude Code hooks) or falls back
    to `cli-<PID>` / `daemon-<PID>` for non-hook invocations.
  - Log files are append-only, named `YYYY-MM-DD.log` under `.brain/logs/`.
- **Done when:** every request records the full metric set; `brain stats` shows
  total savings, avg latency, api-vs-local split, cache hit rate. ✅

---

## Phase 10 — Hardening & packaging 🟢

- **Objective:** Install script, cross-platform checks, robust error handling,
  multi-session locking, docs, CI.
- **Files:** `install.sh` (new), `.github/workflows/ci.yml` (new),
  `README.md` (rewritten — full coverage of all 9 phases),
  `Cargo.toml` (v0.10.0, release profile hardened with `strip`+`codegen-units=1`),
  `crates/brain-cli/src/commands/install_hooks.rs` (replaced bare `.unwrap()`
  with documented `.expect()` on post-invariant-check paths).
- **Tech:** Rust release build, Bash install script, GitHub Actions.
- **Architecture notes:**
  - `install.sh` supports `PREFIX` override, `--uninstall`, and emits a PATH
    hint when the install directory is not on `$PATH`.
  - CI matrix: `stable` + MSRV `1.80` for tests; single `stable` run for fmt,
    clippy, and the release-build smoke test.
  - `clippy --all-targets -- -D warnings` passes clean on the full workspace.
  - Multi-session locking was already handled by WAL + busy-timeout (Phase 1);
    confirmed no additional locking needed.
- **Done when:** one-command install (`./install.sh`); CI runs `fmt` + `clippy`
  + `test`; documented upgrade/migration path; `cargo test` green (103 pass,
  2 ignored for model-download tests). ✅
