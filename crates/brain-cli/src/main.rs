//! `brain` — command-line interface for the Brain Engine.
//!
//! Phase 1: `brain init`, `brain status`
//! Phase 2: `brain index` (chunk indexing)
//! Phase 3: `brain index` now also embeds chunks and writes to LanceDB
//! Phase 4: `brain query` — semantic search + context assembly
//! Phase 6: `brain cache` — inspect and manage the response cache
//! Phase 7: `brain daemon` — manage the long-lived daemon process
//! Phase 8: `brain install-hooks` — generate Claude Code hooks
//! Phase 9: `brain stats` — per-request metrics and daily JSON logs

mod commands;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use commands::cache::CacheAction;
use commands::daemon::DaemonAction;
use commands::stats::StatsAction;

/// Top-level CLI definition.
#[derive(Debug, Parser)]
#[command(
    name = "brain",
    version,
    about = "Brain Engine — local AI context layer for Claude Code",
    propagate_version = true
)]
struct Cli {
    /// Project root to operate on (defaults to the current directory).
    #[arg(long, short = 'C', global = true, value_name = "DIR")]
    path: Option<PathBuf>,

    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// All subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Initialise (or repair) the brain for a project. Safe to run repeatedly.
    Init,
    /// Scan the project, rebuild the chunk index, and update embeddings.
    Index {
        /// Force all chunks to be re-embedded even if vectors already exist.
        /// Also triggered automatically when the embedding model changes.
        #[arg(long)]
        reindex: bool,
        /// Skip the embedding step (index chunks only, no vector store update).
        /// Useful for quick re-scans or when the embedding model is unavailable.
        #[arg(long)]
        no_embed: bool,
    },
    /// Semantic search: embed a query and return the most relevant code chunks.
    Query {
        /// The natural-language question to search for.
        query: String,
        /// Number of ANN candidates to retrieve from the vector store.
        #[arg(long, default_value = "5")]
        top_k: usize,
        /// Maximum tokens to include in the assembled context.
        #[arg(long, default_value = "4000")]
        tokens: usize,
        /// Bypass the response cache for this query (neither reads nor writes).
        #[arg(long)]
        no_cache: bool,
    },
    /// Inspect and manage the response cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Manage the long-lived daemon (start / stop / status / query via socket).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Show the status of the global and project brains.
    Status,
    /// Generate Claude Code hooks for transparent context injection and response caching.
    InstallHooks,
    /// Show per-request metrics (latency, tokens saved, cache hit rate, embedding source).
    Stats {
        #[command(subcommand)]
        action: Option<StatsAction>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let root = match brain_core::resolve_root(cli.path.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("brain: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match cli.command {
        Command::Init => commands::init::run(&root, cli.json),
        Command::Index { reindex, no_embed } => {
            commands::index::run(&root, cli.json, reindex, no_embed)
        }
        Command::Query {
            query,
            top_k,
            tokens,
            no_cache,
        } => commands::query::run(&root, &query, top_k, tokens, cli.json, no_cache),
        Command::Cache { action } => commands::cache::run(&root, cli.json, action),
        Command::Daemon { action } => commands::daemon::run(&root, cli.json, action),
        Command::Status => commands::status::run(&root, cli.json),
        Command::InstallHooks => commands::install_hooks::run(&root, cli.json),
        Command::Stats { action } => {
            let action = action.unwrap_or(StatsAction::Show);
            commands::stats::run(&root, cli.json, action)
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("brain: {e}");
            ExitCode::FAILURE
        }
    }
}
