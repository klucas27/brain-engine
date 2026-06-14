# Brain Engine

Local AI context layer for Claude Code. Indexes a project once and feeds Claude
only the few most relevant chunks per prompt — fewer tokens, lower latency,
fewer hallucinations, zero friction.

See [`ARCHITECTURE.md`](./ARCHITECTURE.md) for the design and
[`IMPLEMENTATION_PLAN.md`](./IMPLEMENTATION_PLAN.md) for the phase roadmap.

> **Status:** All 9 phases implemented and shipped. Phase 10 (hardening &
> packaging) adds this one-command install, the CI pipeline and full docs.

---

## Quick install

```sh
git clone <repo-url> brain-engine
cd brain-engine
./install.sh          # builds a release binary and places it in ~/.local/bin
```

Then add `~/.local/bin` to your `PATH` if it isn't already:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc   # or ~/.zshrc
source ~/.bashrc
```

### Custom prefix

```sh
PREFIX=/usr/local ./install.sh     # installs to /usr/local/bin/brain
```

### Uninstall

```sh
./install.sh --uninstall
```

### Build from source (no install)

```sh
cargo build --release              # binary at target/release/brain
cargo test                         # run the full test suite
```

---

## Concepts

Two *brains* coexist and complement each other:

| Brain | Location | Holds |
|-------|----------|-------|
| Global | `~/.brain/` | Shared config, providers, cross-project cache, long-term memory, daily logs |
| Project | `<root>/.brain/` | This project's index, embeddings, cache and metrics DB |

The project brain is always git-ignored. Multiple Claude Code sessions on the
same project share one project brain via WAL-mode SQLite + a long-lived daemon.

---

## Usage reference

### Initialise

```sh
brain init                     # init global + project brain (idempotent)
brain init -C /path/to/project # operate on a specific project
brain status                   # report health of both brains
brain --json status            # machine-readable JSON
```

### Index

```sh
brain index                    # walk project, chunk files, build embeddings
brain index --reindex          # force full re-embed (e.g. after model change)
brain index --no-embed         # chunk only, skip vector store update
```

`brain index` is incremental: a no-change re-run does ~zero work; only
changed/new/deleted files are touched.

Indexing rules:
- Respects `.gitignore` plus per-project `include_globs` / `exclude_globs`.
- Skips secrets (`.env`, `*.pem`, `*.key`), vendored deps (`node_modules`,
  `target/`, …), binaries (NUL-byte heuristic) and files over
  `max_file_size_bytes`.
- Splits files into overlapping line windows (`chunk.max_lines` /
  `chunk.overlap_lines`).
- Parallelised across cores with Rayon.

### Query

```sh
brain query "how does the auth middleware work?"
brain query "database schema" --top-k 10 --tokens 8000
brain query "..." --no-cache          # bypass the cache for this call
brain --json query "..."              # JSON output
```

Returns the most relevant code chunks with file/line citations and a token
report.

### Cache

```sh
brain cache stats              # show cache size, hit count, live entries
brain cache clear              # wipe all cached responses
brain cache purge              # remove only expired entries
```

The cache is exact (SHA-256 key) by default. An optional semantic cache
(cosine-similarity matching) can be enabled per project:

```json
// brain.config.json
{
  "cache": {
    "semantic_enabled": true,
    "semantic_threshold": 0.92
  }
}
```

### Daemon

```sh
brain daemon start             # start the background daemon
brain daemon status            # check if the daemon is running
brain daemon stop              # stop the daemon
brain daemon query "..."       # query via the daemon socket (for scripts)
```

The daemon keeps the embedding model warm and drives the file watcher. Multiple
Claude sessions share one daemon per project. PID is written to
`.brain/brain.pid`; socket at `.brain/brain.sock`.

### Hooks (Claude Code integration)

```sh
brain install-hooks            # generate .claude/hooks/{pre_prompt,post_response}.sh
```

Once installed, every Claude Code prompt automatically retrieves the most
relevant context and injects it before the model sees your question. Responses
are stored for future cache hits. No manual steps required per session.

The hooks use the `UserPromptSubmit` and `Stop` Claude Code hook events. Context
injection is additive (prepended to the prompt) and never rewrites the model's
full input.

### Stats / metrics

```sh
brain stats                    # show aggregated request metrics
brain stats reset              # clear all recorded metrics
brain --json stats             # JSON output
```

Metrics recorded per request:
- Response time (ms), cache hit/miss
- Context tokens injected (the **real cost**), efficiency ratio
- Embedding source (local vs. API)
- CPU %, RAM (MB) at decision time, decision reason

#### Token metrics: real cost vs. theoretical reduction

Brain Engine reports two *different* token numbers — keep them straight:

| Metric | Meaning | Accumulated? |
|--------|---------|--------------|
| **Real cost** (`real_cost` = `context_tokens`) | Tokens actually injected into the prompt this request. This is what you pay for and the figure to optimise against. | ✅ Yes — `brain stats` sums it as `total_real_tokens` and shows an average per request. |
| **Theoretical reduction** (`reduction_pct`) | How much smaller the injected context is vs. the hypothetical "paste the entire project" baseline. Informative only. | ❌ **Never.** The project size is a fixed baseline, so summing per-request "savings" inflates without bound (you'd "save" more than the project's total size). |
| **Efficiency ratio** (`efficiency_ratio` = `context / project`) | What fraction of the project had to be used. Lower is better. | ❌ No (per-request gauge). |

> ⚠️ Earlier versions headlined a `Saved: ~98k` figure and **accumulated** it per
> session. That number was arithmetically correct but conceptually misleading: it
> compared against a dump that never happens, double-counted the same baseline every
> request, and ignored that retrieved context is *added* to the prompt. It has been
> replaced by the real-cost metric above.

Daily JSON logs are appended to `.brain/logs/YYYY-MM-DD.log` and parseable with
`jq` without needing SQLite. The `tokens_saved_estimated` field is still written
per line (for reference / backward compatibility) but is **not** summed anywhere.

---

## Configuration

### Global config — `~/.brain/config.json`

Controls defaults, decision-engine thresholds, cache TTL and session settings.
Key fields:

```json
{
  "cache": {
    "ttl_seconds": 86400,
    "semantic_enabled": false,
    "semantic_threshold": 0.92
  },
  "decision": {
    "cpu_threshold_pct": 80,
    "ram_threshold_mb": 1024,
    "local_batch_limit": 64
  }
}
```

### Provider config — `~/.brain/providers.json`

Declares embedding and LLM providers. Brain Engine ships with a local provider
(bge-small via ONNX). API providers (DeepSeek, OpenAI) require an `api_key`:

```json
{
  "providers": [
    { "id": "local",   "kind": "local",   "model": "bge-small-en-v1.5" },
    { "id": "deepseek","kind": "deepseek","model": "deepseek-embeddings","api_key": "…" }
  ],
  "default": "local"
}
```

### Per-project config — `<root>/brain.config.json`

```json
{
  "include_globs": ["src/**", "tests/**", "*.toml"],
  "exclude_globs": ["target/**", "node_modules/**"],
  "chunk": { "max_lines": 60, "overlap_lines": 10 },
  "max_file_size_bytes": 524288,
  "embedding_provider": "local"
}
```

---

## Decision engine

Brain Engine never sends secrets to an external API. The decision engine picks
local vs. API processing from live CPU/RAM + batch size + config thresholds and
always emits a `decision_reason` for auditability:

| Condition | Route |
|-----------|-------|
| CPU > `cpu_threshold_pct` | API embeddings |
| RAM available < `ram_threshold_mb` | API embeddings |
| Batch size ≤ `local_batch_limit` | local embeddings |
| Otherwise | local embeddings |

---

## Filesystem layout

```
~/.brain/
  config.json              # global config
  providers.json           # embedding/LLM provider declarations
  cache/                   # cross-project cache (future)
  memory/                  # long-term memory store (future)
  logs/                    # daily JSON logs

<root>/
  brain.config.json        # per-project config
  .brain/                  # project brain (git-ignored)
    metadata.db            # SQLite: files, chunks, cache, sessions, requests
    vectors/               # LanceDB vector store
    cache/                 # project-scoped response cache
    summaries/             # file-level summaries (future)
    logs/                  # per-project daily JSON logs
    brain.sock             # Unix socket (daemon)
    brain.pid              # daemon PID
  .claude/
    hooks/
      pre_prompt.sh        # UserPromptSubmit hook (context injection)
      post_response.sh     # Stop hook (store response + metrics)
```

---

## Upgrade and migration

### Upgrading Brain Engine

```sh
git pull
./install.sh               # re-builds and re-installs the binary
```

The metadata database schema is forward-only and versioned via
`PRAGMA user_version`. No data is lost on upgrade.

### Embedding model change

If you switch the embedding provider or model in `providers.json`, the next
`brain index` detects the dimension mismatch and automatically runs a full
reindex (`--reindex` is implied). This is a safe, atomic operation: the old
vectors remain until the new ones are written.

### Resetting a project

```sh
rm -rf .brain              # wipe the project brain entirely
brain init                 # re-scaffold
brain index                # reindex from scratch
```

### Resetting metrics only

```sh
brain stats reset
```

---

## Security

- `.gitignore` and secret-pattern filters run before any content is indexed.
- Files matching `*.env`, `*.pem`, `*.key`, `*.p12`, `*.pfx` and similar
  patterns are skipped unconditionally.
- Content is only sent to an external API when explicitly configured and when the
  decision engine chooses the API path. The default provider is local.
- The daemon socket (`.brain/brain.sock`) is a local Unix socket; no network
  exposure.

---

## Troubleshooting

**`brain: no brain found — run brain init first`**
Run `brain init` in the project root.

**Daemon won't start / socket in use**
```sh
brain daemon status
brain daemon stop
brain daemon start
```
If the PID file is stale (e.g. after a crash), delete `.brain/brain.pid` and
`.brain/brain.sock` manually, then restart.

**Embeddings not updating after a file change**
The daemon debounces file-system events by 500 ms. If running without the
daemon, run `brain index` explicitly.

**`brain query` returns no results**
Run `brain index` first. Check `brain status` to confirm chunk and vector counts
are non-zero.

---

## Development

```sh
cargo build            # debug build
cargo test             # full test suite (all workspace crates)
cargo fmt              # format
cargo clippy           # lint (CI enforces -D warnings)
cargo build --release  # release build
```

CI runs on every push/PR and covers `fmt`, `clippy`, `cargo test`, and a release
build smoke test. See `.github/workflows/ci.yml`.

---

## License

MIT.
