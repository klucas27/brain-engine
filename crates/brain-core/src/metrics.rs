//! Metrics recording and aggregation — Phase 9.
//!
//! Two persistence layers run in parallel:
//!
//! * **SQLite** (`requests` / `sessions` tables, already in the schema) — durable,
//!   queryable, and survives restarts.  All aggregation for `brain stats` reads
//!   from here.
//!
//! * **JSON log files** (`.brain/logs/YYYY-MM-DD.log`) — one JSON line per request,
//!   append-only.  Useful for external tooling (jq, grep, log shippers) without
//!   needing SQLite.
//!
//! ## Session model
//!
//! A *session* maps to one continuous Claude Code session (via the hook's
//! `CLAUDE_SESSION_ID` env var) or to a single `brain query` CLI invocation.
//! [`ensure_session`] upserts the session row; [`end_session`] sets `ended_at`.
//!
//! ## Log format
//!
//! Each line is a compact JSON object:
//! ```json
//! {"ts":"2026-06-13T10:15:23Z","session_id":"abc","response_time_ms":42,"cache_hit":false,...}
//! ```

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::Result;

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs() as i64
}

/// Format an ISO-8601 UTC timestamp from a UNIX epoch (seconds).
fn iso8601(secs: i64) -> String {
    // Manual formatting avoids an extra dependency on chrono/time.
    // 86400 s/day, 3600 s/hour, 60 s/min
    let s = secs as u64;
    let secs_of_day = s % 86400;
    let days = s / 86400;

    // Days since 1970-01-01 → Gregorian calendar
    let (year, month, day) = days_to_ymd(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Gregorian calendar conversion (days since Unix epoch → Y/M/D).
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Algorithm from Henry F. Fliegel & Thomas C. Van Flandern (1968).
    let jd = days as i64 + 2_440_588; // Julian day number for 1970-01-01
    let l = jd + 68_569;
    let n = 4 * l / 146_097;
    let l = l - (146_097 * n + 3) / 4;
    let i = 4000 * (l + 1) / 1_461_001;
    let l = l - 1461 * i / 4 + 31;
    let j = 80 * l / 2447;
    let day = l - 2447 * j / 80;
    let l = j / 11;
    let month = j + 2 - 12 * l;
    let year = 100 * (n - 49) + i + l;
    (year as u32, month as u32, day as u32)
}

/// `YYYY-MM-DD` string for the current UTC day (used to name log files).
fn today_label() -> String {
    let secs = now_secs();
    let days = secs as u64 / 86400;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// All metrics collected for a single intercepted request.
///
/// Every field is `Option` so callers can populate only what they know.
/// `record_request` stores `NULL` for missing values.
#[derive(Debug, Default, Clone)]
pub struct RequestMetric {
    /// Claude Code session id (from `$CLAUDE_SESSION_ID`), or a generated id
    /// for direct `brain query` invocations.
    pub session_id: Option<String>,
    /// Wall-clock time for the full request (embed + ANN + context), ms.
    pub response_time_ms: Option<u64>,
    /// Token count of the assembled context that was returned.
    pub context_tokens_estimated: Option<i64>,
    /// Estimated tokens saved vs. sending the full project.
    pub tokens_saved_estimated: Option<i64>,
    /// Number of chunks included in the context.
    pub chunks_used: Option<i64>,
    /// Time spent in ANN retrieval alone, ms.
    pub retrieval_time_ms: Option<u64>,
    /// Which embedding backend was used: `"local"` or `"api"`.
    pub embedding_source: Option<String>,
    /// Which LLM was routed to (if recorded by the router).
    pub llm_used: Option<String>,
    /// CPU usage percent at decision time.
    pub cpu_usage_percent: Option<f64>,
    /// RAM used (MiB) at decision time.
    pub memory_usage_mb: Option<i64>,
    /// Human-readable reason from the decision engine.
    pub decision_reason: Option<String>,
    /// Whether a cache hit was returned instead of a live retrieval.
    pub cache_hit: bool,
}

/// Aggregated statistics across all recorded requests.
#[derive(Debug, Default, Clone)]
pub struct StatsReport {
    /// Total requests ever recorded.
    pub total_requests: i64,
    /// Total distinct sessions recorded.
    pub total_sessions: i64,
    /// Number of requests that returned a cache hit.
    pub cache_hits: i64,
    /// `cache_hits / total_requests` (0.0 if no requests).
    pub cache_hit_rate: f64,
    /// Mean request latency in milliseconds (0.0 if no timing data).
    pub avg_latency_ms: f64,
    /// **Real** token cost accumulated across all requests: the sum of the
    /// context actually injected into prompts (`SUM(context_tokens_estimated)`).
    ///
    /// This replaces the old `total_tokens_saved`, which summed a fixed-baseline
    /// theoretical figure and produced meaningless inflated totals.
    pub total_real_tokens: i64,
    /// Requests that used the local embedding backend.
    pub local_count: i64,
    /// Requests that used the remote API embedding backend.
    pub api_count: i64,
}

impl StatsReport {
    /// Mean real token cost per request (0.0 if no requests).
    pub fn avg_real_tokens_per_request(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.total_real_tokens as f64 / self.total_requests as f64
        }
    }

    /// Fraction of embedding requests that used the local backend (0.0–1.0).
    pub fn local_pct(&self) -> f64 {
        let total = self.local_count + self.api_count;
        if total == 0 {
            0.0
        } else {
            self.local_count as f64 / total as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// Ensure a session row exists.  Safe to call multiple times for the same id.
pub fn ensure_session(conn: &Connection, session_id: &str) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT OR IGNORE INTO sessions (id, started_at) VALUES (?1, ?2)",
        rusqlite::params![session_id, now],
    )?;
    Ok(())
}

/// Mark a session as ended by setting `ended_at = now`.
pub fn end_session(conn: &Connection, session_id: &str) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
        rusqlite::params![now, session_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Request recording
// ---------------------------------------------------------------------------

/// Insert one row into `requests` and append a JSON line to the daily log.
///
/// Both operations are best-effort from the caller's perspective — metrics
/// must never break the query path.  Log failures are silently swallowed.
///
/// If `metric.session_id` is `Some`, the session row is upserted automatically
/// (callers do not need a separate [`ensure_session`] call).
pub fn record_request(conn: &Connection, log_dir: &Path, metric: &RequestMetric) -> Result<()> {
    // Ensure the session row exists so the FK constraint is satisfied.
    if let Some(ref sid) = metric.session_id {
        ensure_session(conn, sid)?;
    }

    let now = now_secs();
    let cache_hit_int: i64 = if metric.cache_hit { 1 } else { 0 };

    conn.execute(
        "INSERT INTO requests (
             session_id, timestamp, response_time_ms, context_tokens_estimated,
             tokens_saved_estimated, chunks_used, retrieval_time_ms,
             embedding_source, llm_used, cpu_usage_percent, memory_usage_mb,
             decision_reason, cache_hit
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        rusqlite::params![
            metric.session_id,
            now,
            metric.response_time_ms.map(|v| v as i64),
            metric.context_tokens_estimated,
            metric.tokens_saved_estimated,
            metric.chunks_used,
            metric.retrieval_time_ms.map(|v| v as i64),
            metric.embedding_source,
            metric.llm_used,
            metric.cpu_usage_percent,
            metric.memory_usage_mb,
            metric.decision_reason,
            cache_hit_int,
        ],
    )?;

    // Best-effort append to daily JSON log.
    let _ = append_log(log_dir, metric, now);
    Ok(())
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Read and aggregate all recorded requests into a [`StatsReport`].
pub fn aggregate_stats(conn: &Connection) -> Result<StatsReport> {
    // NOTE: we deliberately do NOT sum `tokens_saved_estimated` — that column
    // stores a fixed-baseline theoretical figure and accumulating it is meaningless.
    // The accumulated metric is the *real* injected cost (`context_tokens_estimated`).
    let report: StatsReport = conn.query_row(
        "SELECT
             COUNT(*)                                                     AS total_requests,
             SUM(cache_hit)                                               AS cache_hits,
             COALESCE(AVG(CAST(response_time_ms AS REAL)), 0.0)           AS avg_latency_ms,
             COALESCE(SUM(context_tokens_estimated), 0)                   AS total_real_tokens,
             COALESCE(SUM(CASE WHEN embedding_source = 'local' THEN 1 ELSE 0 END), 0) AS local_count,
             COALESCE(SUM(CASE WHEN embedding_source = 'api'   THEN 1 ELSE 0 END), 0) AS api_count
         FROM requests",
        [],
        |r| {
            Ok(StatsReport {
                total_requests:    r.get(0)?,
                cache_hits:        r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                avg_latency_ms:    r.get(2)?,
                total_real_tokens: r.get(3)?,
                local_count:       r.get(4)?,
                api_count:         r.get(5)?,
                ..Default::default()
            })
        },
    )?;

    let total_sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;

    let cache_hit_rate = if report.total_requests > 0 {
        report.cache_hits as f64 / report.total_requests as f64
    } else {
        0.0
    };

    Ok(StatsReport {
        total_sessions,
        cache_hit_rate,
        ..report
    })
}


/// Delete all rows in `requests` and `sessions`.  Used by `brain stats --reset`.
pub fn reset(conn: &Connection) -> Result<(i64, i64)> {
    let req: i64 = conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))?;
    let ses: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    conn.execute("DELETE FROM requests", [])?;
    conn.execute("DELETE FROM sessions", [])?;
    Ok((req, ses))
}

// ---------------------------------------------------------------------------
// JSON log writer
// ---------------------------------------------------------------------------

/// Append one JSON line to `.brain/logs/YYYY-MM-DD.log`.
///
/// The function is intentionally infallible from the caller's view (errors are
/// only returned so that the test suite can inspect them; production callers
/// ignore the result).
pub fn append_log(
    log_dir: &Path,
    metric: &RequestMetric,
    timestamp_secs: i64,
) -> std::io::Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(log_dir)?;
    let file_name = format!("{}.log", today_label());
    let log_path = log_dir.join(file_name);

    let ts = iso8601(timestamp_secs);
    // Build the JSON line manually to avoid pulling in serde_json here
    // (brain-core already has it, so we use it).
    let line = build_log_line(&ts, metric);

    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn build_log_line(ts: &str, m: &RequestMetric) -> String {
    use serde_json::{json, Value};

    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), Value::String(ts.to_string()));

    macro_rules! opt_field {
        ($key:expr, $val:expr) => {
            if let Some(v) = $val {
                obj.insert($key.into(), json!(v));
            }
        };
    }

    opt_field!("session_id", m.session_id.as_deref());
    opt_field!("response_time_ms", m.response_time_ms);
    opt_field!("context_tokens_estimated", m.context_tokens_estimated);
    opt_field!("tokens_saved_estimated", m.tokens_saved_estimated);
    opt_field!("chunks_used", m.chunks_used);
    opt_field!("retrieval_time_ms", m.retrieval_time_ms);
    opt_field!("embedding_source", m.embedding_source.as_deref());
    opt_field!("llm_used", m.llm_used.as_deref());
    opt_field!("cpu_usage_percent", m.cpu_usage_percent);
    opt_field!("memory_usage_mb", m.memory_usage_mb);
    opt_field!("decision_reason", m.decision_reason.as_deref());
    obj.insert("cache_hit".into(), Value::Bool(m.cache_hit));

    Value::Object(obj).to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::paths::ProjectPaths;
    use crate::scaffold;

    fn setup() -> (tempfile::TempDir, rusqlite::Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let project = ProjectPaths::new(tmp.path().to_path_buf());
        let mut report = scaffold::InitReport::default();
        scaffold::init_project(&project, &mut report).unwrap();
        let conn = db::open(&project.metadata_db()).unwrap();
        (tmp, conn)
    }

    fn basic_metric() -> RequestMetric {
        RequestMetric {
            session_id: Some("test-session".into()),
            response_time_ms: Some(42),
            context_tokens_estimated: Some(1200),
            tokens_saved_estimated: Some(28800),
            chunks_used: Some(5),
            retrieval_time_ms: Some(38),
            embedding_source: Some("local".into()),
            cache_hit: false,
            ..Default::default()
        }
    }

    #[test]
    fn record_and_aggregate_single_request() {
        let (tmp, conn) = setup();
        let log_dir = tmp.path().join("logs");

        ensure_session(&conn, "test-session").unwrap();
        record_request(&conn, &log_dir, &basic_metric()).unwrap();

        let stats = aggregate_stats(&conn).unwrap();
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.total_sessions, 1);
        assert_eq!(stats.cache_hits, 0);
        // Real cost = sum of context_tokens_estimated (1200), NOT the theoretical saved.
        assert_eq!(stats.total_real_tokens, 1200);
        assert_eq!(stats.avg_real_tokens_per_request(), 1200.0);
        assert_eq!(stats.local_count, 1);
        assert_eq!(stats.api_count, 0);
        assert!((stats.avg_latency_ms - 42.0).abs() < 1.0);
    }

    #[test]
    fn cache_hit_counted_correctly() {
        let (tmp, conn) = setup();
        let log_dir = tmp.path().join("logs");

        ensure_session(&conn, "s1").unwrap();
        let mut m = basic_metric();
        m.cache_hit = true;
        record_request(&conn, &log_dir, &m).unwrap();
        record_request(&conn, &log_dir, &basic_metric()).unwrap();

        let stats = aggregate_stats(&conn).unwrap();
        assert_eq!(stats.total_requests, 2);
        assert_eq!(stats.cache_hits, 1);
        assert!((stats.cache_hit_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn multiple_sessions_counted() {
        let (tmp, conn) = setup();
        let log_dir = tmp.path().join("logs");

        ensure_session(&conn, "s1").unwrap();
        ensure_session(&conn, "s2").unwrap();
        // Calling ensure_session twice with the same id is idempotent.
        ensure_session(&conn, "s1").unwrap();

        let mut m = basic_metric();
        m.session_id = Some("s1".into());
        record_request(&conn, &log_dir, &m).unwrap();

        m.session_id = Some("s2".into());
        m.embedding_source = Some("api".into());
        record_request(&conn, &log_dir, &m).unwrap();

        let stats = aggregate_stats(&conn).unwrap();
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.local_count, 1);
        assert_eq!(stats.api_count, 1);
    }

    #[test]
    fn reset_clears_all_metrics() {
        let (tmp, conn) = setup();
        let log_dir = tmp.path().join("logs");

        // record_request auto-creates the session; no separate ensure_session needed.
        record_request(&conn, &log_dir, &basic_metric()).unwrap();
        assert_eq!(aggregate_stats(&conn).unwrap().total_requests, 1);

        let (req, ses) = reset(&conn).unwrap();
        assert_eq!(req, 1);
        assert_eq!(ses, 1); // only "test-session" was created
        assert_eq!(aggregate_stats(&conn).unwrap().total_requests, 0);
    }

    #[test]
    fn log_file_created_and_contains_valid_json() {
        let (tmp, conn) = setup();
        let log_dir = tmp.path().join("logs");

        ensure_session(&conn, "log-session").unwrap();
        record_request(&conn, &log_dir, &basic_metric()).unwrap();

        // Log dir and at least one file should exist.
        let entries: Vec<_> = std::fs::read_dir(&log_dir).unwrap().collect();
        assert!(
            !entries.is_empty(),
            "log directory should have at least one file"
        );

        // Read the file and verify JSON is parseable.
        let log_file = entries[0].as_ref().unwrap().path();
        let content = std::fs::read_to_string(&log_file).unwrap();
        for line in content.lines().filter(|l| !l.is_empty()) {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("each log line must be valid JSON");
        }
    }

    #[test]
    fn end_session_sets_ended_at() {
        let (_tmp, conn) = setup();
        ensure_session(&conn, "sess-x").unwrap();
        end_session(&conn, "sess-x").unwrap();

        let ended_at: Option<i64> = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE id = 'sess-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            ended_at.is_some(),
            "ended_at should be set after end_session"
        );
    }

    #[test]
    fn iso8601_formats_known_timestamp() {
        // Unix epoch 0 → 1970-01-01T00:00:00Z
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2000-01-01 00:00:00 UTC = 946_684_800
        assert_eq!(iso8601(946_684_800), "2000-01-01T00:00:00Z");
        // 2026-06-13 00:00:00 UTC = 1_781_308_800
        assert_eq!(iso8601(1_781_308_800), "2026-06-13T00:00:00Z");
    }
}
