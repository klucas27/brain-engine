//! `brain-daemon` — long-lived process serving query/index requests.
//!
//! ## Startup sequence
//! 1. Parse `--root <path>` argument
//! 2. Check for a live daemon (try to connect to the socket); exit if found
//! 3. Remove stale PID / socket files from a previous dead daemon
//! 4. Spawn the blocking worker thread (loads models, opens DB)
//! 5. Bind the Unix domain socket at `<root>/.brain/brain.sock`
//! 6. Write our PID to `<root>/.brain/brain.pid`
//! 7. Start the file watcher (debounce → incremental reindex)
//! 8. Accept client connections in a tokio loop
//! 9. On SIGTERM / Ctrl-C: remove PID + socket files, send Shutdown to worker
//!
//! ## Protocol
//! See [`protocol`] module.  One JSON line per message, newline-terminated.

mod protocol;
mod watcher;
mod worker;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc as std_mpsc;

use brain_core::paths::ProjectPaths;
use protocol::{IndexParams, QueryParams, RequestEnvelope, ResponseEnvelope, StoreParams, SymbolsParams};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use worker::WorkerMsg;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    let root = match parse_root() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("brain-daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = run(root).await {
        eprintln!("brain-daemon: {e}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn parse_root() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--root") => {
            let path = args.next().ok_or("--root requires a path")?;
            let p = PathBuf::from(&path);
            if !p.is_dir() {
                return Err(format!("not a directory: {path}"));
            }
            p.canonicalize()
                .map_err(|e| format!("canonicalize {path}: {e}"))
        }
        Some(arg) => Err(format!(
            "unknown argument: {arg}\nUsage: brain-daemon --root <path>"
        )),
        None => {
            // Default to current directory.
            std::env::current_dir()
                .and_then(|p| p.canonicalize())
                .map_err(|e| format!("current dir: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Main async body
// ---------------------------------------------------------------------------

async fn run(root: PathBuf) -> Result<(), String> {
    let paths = ProjectPaths::new(root.clone());
    let socket_path = paths.socket_file();
    let pid_path = paths.pid_file();

    // ── Guard: is another daemon alive? ──────────────────────────────────────
    if socket_path.exists() {
        if probe_socket(&socket_path).await {
            return Err(format!(
                "daemon already running (socket: {})",
                socket_path.display()
            ));
        }
        // Stale socket from a dead daemon — remove it.
        let _ = std::fs::remove_file(&socket_path);
    }
    let _ = std::fs::remove_file(&pid_path);

    // ── Spawn worker thread (loads models, opens DB) ─────────────────────────
    eprintln!("[brain-daemon] initialising (root: {})", root.display());
    let worker_tx = worker::spawn(root.clone()).map_err(|e| format!("worker init: {e}"))?;
    eprintln!("[brain-daemon] ready");

    // ── Bind Unix socket ──────────────────────────────────────────────────────
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| format!("bind {}: {e}", socket_path.display()))?;

    // ── Write PID file ────────────────────────────────────────────────────────
    let pid = std::process::id();
    std::fs::write(&pid_path, format!("{pid}\n")).map_err(|e| format!("write PID file: {e}"))?;

    eprintln!(
        "[brain-daemon] listening on {} (PID {pid})",
        socket_path.display()
    );

    // ── File watcher (sync debounce thread) ───────────────────────────────────
    let _watcher =
        watcher::start_from_sync(&root, worker_tx.clone()).map_err(|e| format!("watcher: {e}"))?;

    // ── Shutdown signal ───────────────────────────────────────────────────────
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    let shutdown_tx2 = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx2.send(());
    });

    // ── Accept loop ────────────────────────────────────────────────────────────
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let wtx  = worker_tx.clone();
                        let mut srx = shutdown_tx.subscribe();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = srx.recv() => {}
                                _ = handle_connection(stream, wtx) => {}
                            }
                        });
                    }
                    Err(e) => eprintln!("[brain-daemon] accept error: {e}"),
                }
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    eprintln!("[brain-daemon] shutting down");
    let _ = worker_tx.send(WorkerMsg::Shutdown);
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&pid_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(stream: UnixStream, worker_tx: std_mpsc::SyncSender<WorkerMsg>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let resp = dispatch(&line, &worker_tx).await;
        let mut out = serde_json::to_string(&resp).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

async fn dispatch(line: &str, worker_tx: &std_mpsc::SyncSender<WorkerMsg>) -> ResponseEnvelope {
    let env: RequestEnvelope = match serde_json::from_str(line) {
        Ok(e) => e,
        Err(e) => return ResponseEnvelope::err(0, format!("invalid JSON: {e}")),
    };
    let id = env.id;

    match env.method.as_str() {
        "ping" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if worker_tx.send(WorkerMsg::Ping { reply: tx }).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            rx.await.ok();
            ResponseEnvelope::ok(id, serde_json::json!({"pong": true}))
        }

        "status" => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if worker_tx.send(WorkerMsg::Status { reply: tx }).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            match rx.await {
                Ok(Ok(v)) => ResponseEnvelope::ok(id, v),
                Ok(Err(e)) => ResponseEnvelope::err(id, e),
                Err(_) => ResponseEnvelope::err(id, "worker dropped reply"),
            }
        }

        "query" => {
            let params: QueryParams = match serde_json::from_value(env.params) {
                Ok(p) => p,
                Err(e) => return ResponseEnvelope::err(id, format!("bad params: {e}")),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let msg = WorkerMsg::Query {
                query: params.query,
                top_k: params.top_k,
                tokens: params.tokens,
                no_cache: params.no_cache,
                reply: tx,
            };
            if worker_tx.send(msg).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            match rx.await {
                Ok(Ok(v)) => ResponseEnvelope::ok(id, v),
                Ok(Err(e)) => ResponseEnvelope::err(id, e),
                Err(_) => ResponseEnvelope::err(id, "worker dropped reply"),
            }
        }

        "index" => {
            let params: IndexParams = serde_json::from_value(env.params).unwrap_or_default();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let msg = WorkerMsg::Index {
                reindex: params.reindex,
                no_embed: params.no_embed,
                reply: tx,
            };
            if worker_tx.send(msg).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            match rx.await {
                Ok(Ok(v)) => ResponseEnvelope::ok(id, v),
                Ok(Err(e)) => ResponseEnvelope::err(id, e),
                Err(_) => ResponseEnvelope::err(id, "worker dropped reply"),
            }
        }

        "store" => {
            let params: StoreParams = match serde_json::from_value(env.params) {
                Ok(p) => p,
                Err(e) => return ResponseEnvelope::err(id, format!("bad params: {e}")),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let msg = WorkerMsg::Store {
                query: params.query,
                response: params.response,
                reply: tx,
            };
            if worker_tx.send(msg).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            match rx.await {
                Ok(Ok(v)) => ResponseEnvelope::ok(id, v),
                Ok(Err(e)) => ResponseEnvelope::err(id, e),
                Err(_) => ResponseEnvelope::err(id, "worker dropped reply"),
            }
        }

        "symbols" => {
            let params: SymbolsParams = serde_json::from_value(env.params).unwrap_or_default();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let msg = WorkerMsg::Symbols {
                name: params.name,
                kind: params.kind,
                limit: params.limit,
                reply: tx,
            };
            if worker_tx.send(msg).is_err() {
                return ResponseEnvelope::err(id, "worker unavailable");
            }
            match rx.await {
                Ok(Ok(v)) => ResponseEnvelope::ok(id, v),
                Ok(Err(e)) => ResponseEnvelope::err(id, e),
                Err(_) => ResponseEnvelope::err(id, "worker dropped reply"),
            }
        }

        other => ResponseEnvelope::err(id, format!("unknown method: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Try to connect to the socket and send a ping.  Returns `true` if the daemon
/// answers within 2 seconds (i.e., it's alive).
async fn probe_socket(socket_path: &Path) -> bool {
    let connect = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        UnixStream::connect(socket_path),
    )
    .await;

    let mut stream = match connect {
        Ok(Ok(s)) => s,
        _ => return false,
    };

    let ping = "{\"id\":0,\"method\":\"ping\"}\n";
    stream.write_all(ping.as_bytes()).await.is_ok()
}

/// Resolves when SIGTERM or Ctrl-C is received.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv()  => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}
