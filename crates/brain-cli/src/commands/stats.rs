//! `brain stats` — consolidated metrics readout.
//!
//! Reads the `requests` and `sessions` tables from the project SQLite database
//! and emits a human-readable (or JSON) report.
//!
//! Subcommands:
//!   brain stats          — show the aggregated statistics
//!   brain stats --reset  — clear all request/session rows and start fresh

use std::path::Path;

use clap::Subcommand;

use brain_core::db;
use brain_core::metrics;
use brain_core::paths::ProjectPaths;
use brain_core::{BrainError, Result};

/// Available actions under `brain stats`.
#[derive(Debug, Subcommand)]
pub enum StatsAction {
    /// Show aggregated request + session statistics (default when no subcommand given).
    Show,
    /// Clear all recorded metrics and start fresh.
    Reset,
}

/// Entry point for `brain stats [action]`.
pub fn run(root: &Path, json: bool, action: StatsAction) -> Result<()> {
    let project = ProjectPaths::new(root.to_path_buf());

    if !project.is_initialised() {
        return Err(BrainError::Walk(format!(
            "no brain found at {} — run `brain init` first",
            project.brain_dir().display()
        )));
    }

    let conn = db::open(&project.metadata_db())?;

    match action {
        StatsAction::Show => {
            let report = metrics::aggregate_stats(&conn)?;
            if json {
                print_json(&report);
            } else {
                print_human(&report, &project);
            }
        }
        StatsAction::Reset => {
            let (req, ses) = metrics::reset(&conn)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "reset": true,
                        "requests_removed": req,
                        "sessions_removed": ses,
                    })
                );
            } else {
                println!("Stats reset: removed {req} request(s) and {ses} session(s).");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Output formatters
// ---------------------------------------------------------------------------

const RULE: &str = "══════════════════════════════════════════════════════════";

fn print_human(r: &metrics::StatsReport, paths: &ProjectPaths) {
    println!("Brain Engine — Request Statistics");
    println!("{RULE}");
    println!();

    if r.total_requests == 0 {
        println!("  No requests recorded yet.");
        println!();
        println!("  Run `brain query` or use the Claude Code hooks to start collecting metrics.");
        return;
    }

    println!("  Total requests      {}", r.total_requests);
    println!("  Sessions            {}", r.total_sessions);
    println!();

    // Cache
    println!(
        "  Cache hit rate      {:.1}%   ({} / {})",
        r.cache_hit_rate * 100.0,
        r.cache_hits,
        r.total_requests
    );

    // Latency (only show if we have data)
    if r.avg_latency_ms > 0.0 {
        println!("  Avg latency         {:.0} ms", r.avg_latency_ms);
    }
    println!();

    // Real token cost (accumulated). The old "tokens saved" figure was a fixed-
    // baseline theoretical number and is deliberately not summed any more.
    if r.total_real_tokens > 0 {
        println!("  Token cost (real)");
        println!(
            "    total injected    {:>12}",
            fmt_num(r.total_real_tokens)
        );
        println!(
            "    avg per request   {:>12}",
            fmt_num(r.avg_real_tokens_per_request().round() as i64)
        );
    }
    println!();

    // Embedding source split
    let embed_total = r.local_count + r.api_count;
    if embed_total > 0 {
        println!("  Embedding source");
        println!(
            "    local             {:>4}  ({:.1}%)",
            r.local_count,
            r.local_pct() * 100.0
        );
        println!(
            "    api               {:>4}  ({:.1}%)",
            r.api_count,
            (1.0 - r.local_pct()) * 100.0
        );
        println!();
    }

    println!("  Logs: {}", paths.logs_dir().display());
}

fn print_json(r: &metrics::StatsReport) {
    let v = serde_json::json!({
        "total_requests":        r.total_requests,
        "total_sessions":        r.total_sessions,
        "cache_hits":            r.cache_hits,
        "cache_hit_rate":        r.cache_hit_rate,
        "avg_latency_ms":        r.avg_latency_ms,
        "total_real_tokens":     r.total_real_tokens,
        "avg_real_tokens_per_request": r.avg_real_tokens_per_request(),
        "local_embedding_count": r.local_count,
        "api_embedding_count":   r.api_count,
        "local_embedding_pct":   r.local_pct(),
    });
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
}

/// Format a large integer with thousands separators (e.g. 1_234_567 → "1,234,567").
fn fmt_num(n: i64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::fmt_num;

    #[test]
    fn fmt_num_small() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(42), "42");
        assert_eq!(fmt_num(999), "999");
    }

    #[test]
    fn fmt_num_thousands() {
        assert_eq!(fmt_num(1000), "1,000");
        assert_eq!(fmt_num(1_234_567), "1,234,567");
        assert_eq!(fmt_num(28_800), "28,800");
    }
}
