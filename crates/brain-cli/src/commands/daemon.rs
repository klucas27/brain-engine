//! `brain daemon` — manage the long-lived daemon process.
//!
//! Subcommands:
//!   start   — spawn `brain-daemon` in the background for this project
//!   stop    — send SIGTERM to the daemon via its PID file
//!   status  — ping the daemon via the Unix socket
//!   query   — send a query to the running daemon

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use brain_core::paths::ProjectPaths;
use brain_core::BrainError;
use brain_core::Result;

use clap::Subcommand;

// ---------------------------------------------------------------------------
// Subcommand definition
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Start the daemon in the background (idempotent if already running).
    Start,
    /// Stop the daemon by sending SIGTERM.
    Stop,
    /// Check whether the daemon is alive.
    Status,
    /// Send a query to the running daemon.
    Query {
        /// The natural-language question to search for.
        query: String,
        /// Number of ANN candidates to retrieve.
        #[arg(long, default_value = "5")]
        top_k: usize,
        /// Maximum context tokens.
        #[arg(long, default_value = "4000")]
        tokens: usize,
        /// Bypass the cache for this query.
        #[arg(long)]
        no_cache: bool,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(root: &Path, json: bool, action: DaemonAction) -> Result<()> {
    let paths = ProjectPaths::new(root.to_path_buf());
    match action {
        DaemonAction::Start => start(&paths, json),
        DaemonAction::Stop => stop(&paths, json),
        DaemonAction::Status => status(&paths, json),
        DaemonAction::Query {
            query,
            top_k,
            tokens,
            no_cache,
        } => query_cmd(&paths, json, &query, top_k, tokens, no_cache),
    }
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

fn start(paths: &ProjectPaths, json: bool) -> Result<()> {
    // Check if already alive.
    if is_alive(paths) {
        if json {
            println!("{}", serde_json::json!({"status": "already_running"}));
        } else {
            println!("brain-daemon: already running");
        }
        return Ok(());
    }

    let daemon_bin = find_daemon_binary().ok_or_else(|| {
        BrainError::Walk(
            "brain-daemon binary not found next to the `brain` executable. \
             Build with `cargo build` and ensure both binaries are in PATH."
                .to_string(),
        )
    })?;

    let mut child = std::process::Command::new(&daemon_bin)
        .arg("--root")
        .arg(paths.root.as_os_str())
        // Detach stdio so the daemon runs silently in the background.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| BrainError::Walk(format!("spawn brain-daemon: {e}")))?;

    // Wait up to 5 s for the daemon to become reachable.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let socket_path = paths.socket_file();
    loop {
        if socket_path.exists() && is_alive(paths) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // Check if the child exited with an error.
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    return Err(BrainError::Walk(format!(
                        "brain-daemon exited early ({})",
                        status
                            .code()
                            .map_or("unknown".to_string(), |c| c.to_string())
                    )));
                }
                _ => {}
            }
            return Err(BrainError::Walk(
                "timed out waiting for daemon to start".to_string(),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let pid = child.id();
    if json {
        println!("{}", serde_json::json!({"status": "started", "pid": pid}));
    } else {
        println!("brain-daemon started (PID {pid})");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------

fn stop(paths: &ProjectPaths, json: bool) -> Result<()> {
    let pid_path = paths.pid_file();
    if !pid_path.exists() {
        if json {
            println!("{}", serde_json::json!({"status": "not_running"}));
        } else {
            eprintln!("brain-daemon: no PID file — daemon may not be running");
        }
        return Ok(());
    }

    let pid_str = std::fs::read_to_string(&pid_path).map_err(|e| BrainError::io(&pid_path, e))?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .map_err(|_| BrainError::Walk(format!("invalid PID in {}", pid_path.display())))?;

    // Send SIGTERM via the `kill` utility (portable across Unix distros).
    let status = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .map_err(|e| BrainError::Walk(format!("kill: {e}")))?;

    if json {
        println!(
            "{}",
            serde_json::json!({"status": "stopped", "pid": pid, "kill_ok": status.success()})
        );
    } else if status.success() {
        println!("brain-daemon: sent SIGTERM to PID {pid}");
    } else {
        eprintln!(
            "brain-daemon: kill returned {}",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn status(paths: &ProjectPaths, json: bool) -> Result<()> {
    if !paths.socket_file().exists() {
        if json {
            println!(
                "{}",
                serde_json::json!({"running": false, "reason": "no socket file"})
            );
        } else {
            println!("brain-daemon: not running (no socket)");
        }
        return Ok(());
    }

    match socket_request(paths, r#"{"id":1,"method":"status"}"#) {
        Ok(resp) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else if let Some(r) = resp.get("result") {
                println!("brain-daemon: running");
                println!(
                    "  project  {}",
                    r.get("project").and_then(|v| v.as_str()).unwrap_or("?")
                );
                println!(
                    "  files    {}",
                    r.get("files").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                println!(
                    "  chunks   {}",
                    r.get("chunks").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                println!(
                    "  vectors  {}",
                    r.get("vectors").and_then(|v| v.as_i64()).unwrap_or(0)
                );
                println!(
                    "  model    {}",
                    r.get("embedding_model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                );
            } else {
                println!("brain-daemon: running (no status detail)");
            }
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"running": false, "reason": e.to_string()})
                );
            } else {
                println!("brain-daemon: not responding ({e})");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

fn query_cmd(
    paths: &ProjectPaths,
    json: bool,
    query: &str,
    top_k: usize,
    tokens: usize,
    no_cache: bool,
) -> Result<()> {
    let req = serde_json::json!({
        "id": 2,
        "method": "query",
        "params": { "query": query, "top_k": top_k, "tokens": tokens, "no_cache": no_cache }
    });

    match socket_request(paths, &serde_json::to_string(&req).unwrap()) {
        Ok(resp) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            } else {
                print_query_human(&resp);
            }
        }
        Err(e) => return Err(BrainError::Walk(e.to_string())),
    }
    Ok(())
}

fn print_query_human(resp: &serde_json::Value) {
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        eprintln!(
            "error: {}",
            resp.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
        return;
    }
    let result = match resp.get("result") {
        Some(r) => r,
        None => {
            eprintln!("no result");
            return;
        }
    };

    if result.get("cache_hit").and_then(|v| v.as_bool()) == Some(true) {
        let kind = result
            .get("cache_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let r = result
            .get("response")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!("[cache:{kind}] {r}");
        return;
    }

    let chunks = result.get("chunks").and_then(|v| v.as_array());
    let rule = "─────────────────────────────────────────────────────────────────────";

    if chunks.map_or(true, |c| c.is_empty()) {
        println!("No relevant chunks found.");
    } else {
        for c in chunks.unwrap() {
            let fp = c.get("file_path").and_then(|v| v.as_str()).unwrap_or("?");
            let sl = c.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let el = c.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0);
            let sc = c.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let tok = c
                .get("token_estimate")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let txt = c.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let rk = c.get("rank").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("[{rk}] {fp}:{sl}-{el} │ score {sc:.3} │ ~{tok} tokens");
            println!("{rule}");
            println!("{}", txt.trim_end());
            println!();
        }
    }

    if let Some(stats) = result.get("stats") {
        let ct = stats
            .get("context_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pt = stats
            .get("project_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let sv = stats
            .get("savings_pct")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        println!("{rule}");
        println!("Token report");
        println!("  context  {ct} / {pt} project (estimated)");
        println!("  saved    {sv:.1}%");
    }
}

// ---------------------------------------------------------------------------
// Socket communication helpers
// ---------------------------------------------------------------------------

fn is_alive(paths: &ProjectPaths) -> bool {
    socket_request(paths, r#"{"id":0,"method":"ping"}"#).is_ok()
}

fn socket_request(
    paths: &ProjectPaths,
    req: &str,
) -> std::result::Result<serde_json::Value, String> {
    let socket_path = paths.socket_file();
    let mut stream = UnixStream::connect(&socket_path)
        .map_err(|e| format!("connect to {}: {e}", socket_path.display()))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;

    let msg = format!("{req}\n");
    stream
        .write_all(msg.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;

    serde_json::from_str(line.trim()).map_err(|e| format!("parse response: {e}"))
}

// ---------------------------------------------------------------------------
// Binary discovery
// ---------------------------------------------------------------------------

fn find_daemon_binary() -> Option<std::path::PathBuf> {
    // Look next to the current executable first.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("brain-daemon");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // Fall back to PATH.
    which_brain_daemon()
}

fn which_brain_daemon() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            let candidate = dir.join("brain-daemon");
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}
