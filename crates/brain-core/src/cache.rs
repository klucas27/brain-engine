//! Response cache — Phase 6.
//!
//! Provides two complementary caching strategies backed by the `cache` table
//! already present in the SQLite metadata database:
//!
//! ## Exact cache
//!
//! Key: `sha256(query_text + "\0" + model_id)` — deterministic, collision-safe.
//! On a cache hit the stored `response` is returned verbatim and the caller can
//! skip embedding + retrieval entirely.  Entries expire when
//! `UNIXEPOCH() >= expires_at`; expired rows are treated as misses and pruned
//! lazily on the next write.
//!
//! ## Semantic cache (opt-in)
//!
//! When `CacheConfig::semantic_enabled` is `true`, every stored entry also
//! carries the query vector (JSON-serialised `Vec<f32>`).  On lookup we iterate
//! live entries, compute cosine similarity between the incoming vector and each
//! stored vector, and return the best match if it exceeds
//! `CacheConfig::semantic_threshold`.
//!
//! The semantic cache is **opt-in and off by default** because a fuzzy match
//! could silently return a stale or context-mismatched answer.  Users who want
//! it must explicitly set `semantic_enabled = true` in `config.json`.
//!
//! ## Thread safety
//!
//! All operations take a `&Connection`.  SQLite in WAL mode handles concurrent
//! readers; callers are responsible for not sharing `Connection` across threads.
//!
//! ## Purging
//!
//! [`purge_expired`] removes rows whose `expires_at` has passed.  The indexer
//! and daemon will call it on startup; the CLI calls it after a cache store.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::config::CacheConfig;
use crate::error::Result;

// ---------------------------------------------------------------------------
// Public API types
// ---------------------------------------------------------------------------

/// The outcome of a cache lookup.
#[derive(Debug, Clone)]
pub enum CacheLookup {
    /// The cache has a valid, non-expired entry for this key.
    Hit {
        /// The stored response string.
        response: String,
        /// How the hit was found (`"exact"` or `"semantic"`).
        kind: HitKind,
    },
    /// Nothing useful was found; the caller should proceed normally.
    Miss,
}

/// Distinguishes between exact and semantic cache hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// Matched by SHA-256 key equality.
    Exact,
    /// Matched by cosine similarity above the configured threshold.
    Semantic,
}

impl HitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HitKind::Exact => "exact",
            HitKind::Semantic => "semantic",
        }
    }
}

impl std::fmt::Display for HitKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Key derivation
// ---------------------------------------------------------------------------

/// Derive the cache key for an exact lookup: `sha256(query + "\0" + model_id)`.
///
/// The null byte separator prevents a query like `"hello" + model "world"`
/// from colliding with `"hellow" + model "orld"`.
pub fn make_key(query_text: &str, model_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(query_text.as_bytes());
    h.update(b"\x00");
    h.update(model_id.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Timestamp helpers
// ---------------------------------------------------------------------------

/// Current UNIX timestamp in seconds.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// Look up `query_text` + `model_id` in the cache.
///
/// 1. Tries an exact SHA-256 match first.
/// 2. If `cfg.semantic_enabled` and `query_vector` is `Some`, scans live
///    entries for the closest vector match above `cfg.semantic_threshold`.
///
/// Returns [`CacheLookup::Miss`] if nothing qualifies.
pub fn lookup(
    conn: &Connection,
    cfg: &CacheConfig,
    query_text: &str,
    model_id: &str,
    query_vector: Option<&[f32]>,
) -> Result<CacheLookup> {
    let key = make_key(query_text, model_id);
    let now = now_secs();

    // ── 1. Exact hit ────────────────────────────────────────────────────────
    let exact: Option<String> = conn
        .query_row(
            "SELECT response FROM cache
             WHERE key = ?1
               AND (expires_at IS NULL OR expires_at > ?2)",
            rusqlite::params![key, now],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(response) = exact {
        return Ok(CacheLookup::Hit {
            response,
            kind: HitKind::Exact,
        });
    }

    // ── 2. Semantic hit (opt-in) ─────────────────────────────────────────────
    if cfg.semantic_enabled {
        if let Some(qv) = query_vector {
            if let Some(hit) = semantic_lookup(conn, cfg, qv, now)? {
                return Ok(CacheLookup::Hit {
                    response: hit,
                    kind: HitKind::Semantic,
                });
            }
        }
    }

    Ok(CacheLookup::Miss)
}

/// Scan non-expired entries that have a stored vector and return the response
/// with the highest cosine similarity to `query_vector` if it exceeds the
/// configured threshold.
fn semantic_lookup(
    conn: &Connection,
    cfg: &CacheConfig,
    query_vector: &[f32],
    now: i64,
) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT response, query_vector FROM cache
         WHERE query_vector IS NOT NULL
           AND (expires_at IS NULL OR expires_at > ?1)",
    )?;

    let mut best_score = cfg.semantic_threshold;
    let mut best_response: Option<String> = None;

    let rows = stmt.query_map(rusqlite::params![now], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (response, vector_json) = row?;
        // Deserialize stored vector; skip malformed entries gracefully.
        let stored: Vec<f32> = match serde_json::from_str(&vector_json) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if stored.len() != query_vector.len() {
            continue;
        }
        let sim = cosine_similarity(query_vector, &stored);
        if sim > best_score {
            best_score = sim;
            best_response = Some(response);
        }
    }

    Ok(best_response)
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Store a response in the cache.
///
/// * `ttl_override` — if `Some(secs)`, overrides `cfg.ttl_seconds` for this
///   entry (useful for tests or short-lived results).  Pass `None` to use the
///   global TTL.
/// * `query_vector` — when `Some`, the entry is also eligible for semantic
///   lookup by future callers.  Pass `None` for exact-only entries.
///
/// This function uses `INSERT OR REPLACE` so re-storing the same key refreshes
/// the expiry and response atomically.
pub fn store(
    conn: &Connection,
    cfg: &CacheConfig,
    query_text: &str,
    model_id: &str,
    response: &str,
    query_vector: Option<&[f32]>,
    ttl_override: Option<u64>,
) -> Result<()> {
    let key = make_key(query_text, model_id);
    let now = now_secs();
    // Distinguish between "explicit 0-second TTL" (expire immediately) and
    // "config says ttl_seconds = 0" (no expiry / immortal entry).
    let expires_at: Option<i64> = match ttl_override {
        Some(0) => Some(now),                       // expire right now
        Some(n) => Some(now + n as i64),            // explicit TTL in seconds
        None if cfg.ttl_seconds == 0 => None,       // config: no expiry
        None => Some(now + cfg.ttl_seconds as i64), // config TTL
    };

    let vector_json: Option<String> =
        query_vector.map(|v| serde_json::to_string(v).expect("f32 vec is always serializable"));

    conn.execute(
        "INSERT OR REPLACE INTO cache (key, response, created_at, expires_at, query_vector)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![key, response, now, expires_at, vector_json],
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Purge
// ---------------------------------------------------------------------------

/// Delete all expired cache entries.  Returns the number of rows removed.
///
/// Call this on startup or after writing a new entry to keep the table lean.
pub fn purge_expired(conn: &Connection) -> Result<usize> {
    let now = now_secs();
    let deleted = conn.execute(
        "DELETE FROM cache WHERE expires_at IS NOT NULL AND expires_at <= ?1",
        rusqlite::params![now],
    )?;
    Ok(deleted)
}

/// Delete *all* cache entries unconditionally.  Use for `brain cache clear`.
pub fn clear(conn: &Connection) -> Result<usize> {
    let deleted = conn.execute("DELETE FROM cache", [])?;
    Ok(deleted)
}

/// Count live (non-expired) cache entries.
pub fn count_live(conn: &Connection) -> Result<i64> {
    let now = now_secs();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM cache WHERE expires_at IS NULL OR expires_at > ?1",
        rusqlite::params![now],
        |r| r.get(0),
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Cosine similarity (pure Rust, no extra deps)
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two equal-length vectors.
///
/// Returns a value in `[-1, 1]`; 1.0 means identical direction.
/// Returns 0.0 for zero-magnitude inputs to avoid division by zero.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vector length mismatch");
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    dot / (mag_a * mag_b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CacheConfig;
    use crate::db;
    use crate::paths::ProjectPaths;
    use crate::scaffold;

    // Spin up an isolated in-memory-equivalent DB in a temp dir.
    fn setup() -> (tempfile::TempDir, rusqlite::Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let project = ProjectPaths::new(tmp.path().to_path_buf());
        let mut report = scaffold::InitReport::default();
        scaffold::init_project(&project, &mut report).unwrap();
        let conn = db::open(&project.metadata_db()).unwrap();
        (tmp, conn)
    }

    fn cfg_exact() -> CacheConfig {
        CacheConfig {
            ttl_seconds: 3600,
            semantic_enabled: false,
            semantic_threshold: 0.95,
        }
    }

    fn cfg_semantic() -> CacheConfig {
        CacheConfig {
            ttl_seconds: 3600,
            semantic_enabled: true,
            semantic_threshold: 0.90,
        }
    }

    // ── Exact cache ─────────────────────────────────────────────────────────

    #[test]
    fn exact_cache_hit_on_same_query() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        store(
            &conn,
            &cfg,
            "what is foo?",
            "bge-small",
            "answer: foo is bar",
            None,
            None,
        )
        .unwrap();
        let result = lookup(&conn, &cfg, "what is foo?", "bge-small", None).unwrap();
        match result {
            CacheLookup::Hit { response, kind } => {
                assert_eq!(response, "answer: foo is bar");
                assert_eq!(kind, HitKind::Exact);
            }
            CacheLookup::Miss => panic!("expected cache hit"),
        }
    }

    #[test]
    fn exact_cache_miss_on_different_query() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        store(
            &conn,
            &cfg,
            "what is foo?",
            "bge-small",
            "answer",
            None,
            None,
        )
        .unwrap();
        let result = lookup(&conn, &cfg, "what is bar?", "bge-small", None).unwrap();
        assert!(matches!(result, CacheLookup::Miss));
    }

    #[test]
    fn exact_cache_miss_on_different_model() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        store(
            &conn,
            &cfg,
            "what is foo?",
            "bge-small",
            "answer",
            None,
            None,
        )
        .unwrap();
        let result = lookup(&conn, &cfg, "what is foo?", "openai-3-small", None).unwrap();
        assert!(matches!(result, CacheLookup::Miss));
    }

    // ── TTL / expiry ────────────────────────────────────────────────────────

    #[test]
    fn expired_entry_is_a_miss() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        // Store with a TTL of 0 seconds → expires immediately.
        store(&conn, &cfg, "query", "model", "stale", None, Some(0)).unwrap();
        // The entry might expire_at = now; wait isn't necessary because the
        // condition is `expires_at > now`, and a TTL of 0 yields expires_at == now.
        let result = lookup(&conn, &cfg, "query", "model", None).unwrap();
        assert!(
            matches!(result, CacheLookup::Miss),
            "TTL-0 entry should be a miss immediately"
        );
    }

    #[test]
    fn no_ttl_entry_never_expires() {
        let (_tmp, conn) = setup();
        let mut cfg = cfg_exact();
        cfg.ttl_seconds = 0; // signals "no expiry"
        store(&conn, &cfg, "eternal", "model", "forever", None, None).unwrap();
        let result = lookup(&conn, &cfg, "eternal", "model", None).unwrap();
        assert!(
            matches!(result, CacheLookup::Hit { .. }),
            "ttl=0 entry should never expire"
        );
    }

    #[test]
    fn purge_expired_removes_stale_entries() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        // Two entries: one expired, one live.
        store(&conn, &cfg, "stale", "m", "old", None, Some(0)).unwrap();
        store(&conn, &cfg, "fresh", "m", "new", None, None).unwrap();

        let removed = purge_expired(&conn).unwrap();
        assert_eq!(removed, 1, "exactly one expired entry should be removed");
        assert_eq!(count_live(&conn).unwrap(), 1);
    }

    // ── Idempotent store (replace refreshes entry) ───────────────────────────

    #[test]
    fn storing_same_key_twice_replaces_entry() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        store(&conn, &cfg, "q", "m", "v1", None, None).unwrap();
        store(&conn, &cfg, "q", "m", "v2", None, None).unwrap();
        match lookup(&conn, &cfg, "q", "m", None).unwrap() {
            CacheLookup::Hit { response, .. } => assert_eq!(response, "v2"),
            CacheLookup::Miss => panic!("expected hit"),
        }
    }

    // ── clear / count ────────────────────────────────────────────────────────

    #[test]
    fn clear_removes_all_entries() {
        let (_tmp, conn) = setup();
        let cfg = cfg_exact();
        store(&conn, &cfg, "a", "m", "r", None, None).unwrap();
        store(&conn, &cfg, "b", "m", "r", None, None).unwrap();
        assert_eq!(count_live(&conn).unwrap(), 2);
        clear(&conn).unwrap();
        assert_eq!(count_live(&conn).unwrap(), 0);
    }

    // ── Semantic cache ───────────────────────────────────────────────────────

    #[test]
    fn semantic_cache_hit_above_threshold() {
        let (_tmp, conn) = setup();
        let cfg = cfg_semantic();

        let stored_vec: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0];
        // Store with a vector.
        store(
            &conn,
            &cfg,
            "original query",
            "m",
            "semantic answer",
            Some(&stored_vec),
            None,
        )
        .unwrap();

        // Query with a very similar vector (cos sim ≈ 0.9995).
        let query_vec: Vec<f32> = vec![0.999, 0.001, 0.001, 0.0];
        let result = lookup(&conn, &cfg, "different wording", "m", Some(&query_vec)).unwrap();

        match result {
            CacheLookup::Hit { response, kind } => {
                assert_eq!(response, "semantic answer");
                assert_eq!(kind, HitKind::Semantic);
            }
            CacheLookup::Miss => panic!("expected semantic cache hit"),
        }
    }

    #[test]
    fn semantic_cache_miss_below_threshold() {
        let (_tmp, conn) = setup();
        let cfg = cfg_semantic();

        let stored_vec: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0];
        store(
            &conn,
            &cfg,
            "query a",
            "m",
            "answer",
            Some(&stored_vec),
            None,
        )
        .unwrap();

        // Orthogonal vector → cosine sim = 0.0 < 0.90.
        let query_vec: Vec<f32> = vec![0.0, 1.0, 0.0, 0.0];
        let result = lookup(&conn, &cfg, "query b", "m", Some(&query_vec)).unwrap();
        assert!(matches!(result, CacheLookup::Miss));
    }

    #[test]
    fn semantic_cache_disabled_returns_miss() {
        let (_tmp, conn) = setup();
        // Use exact-only config (semantic_enabled = false).
        let cfg = cfg_exact();

        let stored_vec: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0];
        store(
            &conn,
            &cfg,
            "query a",
            "m",
            "answer",
            Some(&stored_vec),
            None,
        )
        .unwrap();

        // Even with a near-identical vector, semantic is disabled.
        let query_vec: Vec<f32> = vec![0.999, 0.001, 0.0, 0.0];
        let result = lookup(&conn, &cfg, "query b", "m", Some(&query_vec)).unwrap();
        assert!(matches!(result, CacheLookup::Miss));
    }

    // ── cosine_similarity ────────────────────────────────────────────────────

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0f32, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6, "expected 1.0, got {sim}");
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6, "expected 0.0, got {sim}");
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        let a = vec![0.0f32, 0.0, 0.0];
        let b = vec![1.0f32, 2.0, 3.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
        assert_eq!(cosine_similarity(&b, &a), 0.0);
    }

    #[test]
    fn make_key_different_model_different_key() {
        let k1 = make_key("hello", "model-a");
        let k2 = make_key("hello", "model-b");
        assert_ne!(k1, k2);
    }

    #[test]
    fn make_key_same_inputs_same_key() {
        let k1 = make_key("hello world", "bge-small");
        let k2 = make_key("hello world", "bge-small");
        assert_eq!(k1, k2);
    }
}
