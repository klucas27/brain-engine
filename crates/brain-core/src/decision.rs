//! Decision engine — local vs API embedding backend selector.
//!
//! The engine samples live CPU % and resident-memory MB from the OS, then
//! applies the deterministic thresholds stored in [`DecisionConfig`] to choose
//! between running embeddings locally (ONNX/fastembed) or delegating them to an
//! external API.
//!
//! # Rules (evaluated in priority order)
//!
//! 1. **High CPU** — if `cpu_pct >= cpu_high_threshold` → `Api`
//!    (local model would contend with whatever is heating the CPU)
//! 2. **High RAM** — if `ram_used_mb >= memory_high_threshold_mb` → `Api`
//!    (loading the ONNX weights would cause swapping)
//! 3. **Large batch** — if `batch_size >= large_batch_threshold` → `Local`
//!    (throughput favours the local path; no per-token API cost)
//! 4. **Default** — `Local`
//!    (baseline: keep everything private and free)
//!
//! # Testability
//!
//! The actual OS sampling is isolated in [`SystemSnapshot::capture`].  All rule
//! logic operates on the plain [`SystemSnapshot`] struct, making unit tests
//! fully deterministic without any mocking infrastructure.

use sysinfo::System;

use crate::config::DecisionConfig;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which embedding backend the decision engine selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingBackend {
    /// Run embeddings locally via ONNX / fastembed.
    Local,
    /// Delegate embeddings to an external API.
    Api,
}

impl EmbeddingBackend {
    /// Returns `"local"` or `"api"` — matches the keys used in `providers.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbeddingBackend::Local => "local",
            EmbeddingBackend::Api => "api",
        }
    }
}

impl std::fmt::Display for EmbeddingBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a single decision: the chosen backend plus a human-readable
/// explanation of *why* it was chosen.  Both fields are always populated.
#[derive(Debug, Clone)]
pub struct Decision {
    /// The selected embedding backend.
    pub backend: EmbeddingBackend,
    /// A short, human-readable explanation of the rule that fired.
    pub reason: String,
}

/// A snapshot of the relevant system metrics at decision time.
///
/// Keeping this as a plain data struct lets tests inject arbitrary values
/// without touching the real OS.
#[derive(Debug, Clone, Copy)]
pub struct SystemSnapshot {
    /// Overall CPU utilisation, 0–100 %.
    pub cpu_pct: u8,
    /// Resident-set size of all processes combined, in MiB.
    pub ram_used_mb: u64,
}

impl SystemSnapshot {
    /// Sample live CPU and RAM from the operating system.
    ///
    /// `sysinfo` requires two refreshes to compute a meaningful CPU % (it
    /// measures usage between two polling points).  We sleep 200 ms between
    /// them — short enough not to block a CLI command noticeably but long
    /// enough for a stable reading.
    pub fn capture() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        // Second refresh after a brief pause gives an accurate CPU delta.
        std::thread::sleep(std::time::Duration::from_millis(200));
        sys.refresh_cpu_all();

        let cpu_pct = sys.global_cpu_usage().round() as u8;
        let ram_used_mb = sys.used_memory() / (1024 * 1024);

        Self {
            cpu_pct,
            ram_used_mb,
        }
    }
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Evaluate the decision rules against `snapshot` and `batch_size`.
///
/// This is the pure, testable core of the engine.  Callers that need the live
/// OS reading should call [`decide`] instead.
pub fn evaluate(cfg: &DecisionConfig, snapshot: SystemSnapshot, batch_size: usize) -> Decision {
    // Rule 1 — high CPU
    if snapshot.cpu_pct >= cfg.cpu_high_threshold {
        return Decision {
            backend: EmbeddingBackend::Api,
            reason: format!(
                "cpu_high: observed {}% >= threshold {}%",
                snapshot.cpu_pct, cfg.cpu_high_threshold
            ),
        };
    }

    // Rule 2 — high RAM
    if snapshot.ram_used_mb >= cfg.memory_high_threshold_mb {
        return Decision {
            backend: EmbeddingBackend::Api,
            reason: format!(
                "ram_high: used {}MB >= threshold {}MB",
                snapshot.ram_used_mb, cfg.memory_high_threshold_mb
            ),
        };
    }

    // Rule 3 — large batch (throughput advantage for local)
    if batch_size >= cfg.large_batch_threshold {
        return Decision {
            backend: EmbeddingBackend::Local,
            reason: format!(
                "large_batch: batch {} >= threshold {} (local throughput wins)",
                batch_size, cfg.large_batch_threshold
            ),
        };
    }

    // Rule 4 — default
    Decision {
        backend: EmbeddingBackend::Local,
        reason: "default: system resources within limits, using local backend".to_string(),
    }
}

/// Sample live system metrics and run the decision engine.
///
/// Prefer [`evaluate`] in tests; use this in production code paths.
pub fn decide(cfg: &DecisionConfig, batch_size: usize) -> Decision {
    let snapshot = SystemSnapshot::capture();
    evaluate(cfg, snapshot, batch_size)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DecisionConfig;

    fn cfg() -> DecisionConfig {
        DecisionConfig {
            cpu_high_threshold: 80,
            memory_high_threshold_mb: 2048,
            large_batch_threshold: 64,
        }
    }

    fn snap(cpu_pct: u8, ram_used_mb: u64) -> SystemSnapshot {
        SystemSnapshot {
            cpu_pct,
            ram_used_mb,
        }
    }

    // ------------------------------------------------------------------
    // Rule 1 — high CPU → Api
    // ------------------------------------------------------------------

    #[test]
    fn high_cpu_routes_to_api() {
        let d = evaluate(&cfg(), snap(85, 512), 1);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("cpu_high"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_exactly_at_threshold_routes_to_api() {
        let d = evaluate(&cfg(), snap(80, 512), 1);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("cpu_high"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_one_below_threshold_does_not_trigger_rule1() {
        let d = evaluate(&cfg(), snap(79, 512), 1);
        // Should NOT be API due to CPU; other rules apply.
        assert!(
            !d.reason.contains("cpu_high"),
            "unexpected cpu_high, reason: {}",
            d.reason
        );
    }

    // ------------------------------------------------------------------
    // Rule 2 — high RAM → Api
    // ------------------------------------------------------------------

    #[test]
    fn high_ram_routes_to_api() {
        let d = evaluate(&cfg(), snap(10, 3000), 1);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("ram_high"), "reason: {}", d.reason);
    }

    #[test]
    fn ram_exactly_at_threshold_routes_to_api() {
        let d = evaluate(&cfg(), snap(10, 2048), 1);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("ram_high"), "reason: {}", d.reason);
    }

    #[test]
    fn ram_one_below_threshold_does_not_trigger_rule2() {
        let d = evaluate(&cfg(), snap(10, 2047), 1);
        assert!(
            !d.reason.contains("ram_high"),
            "unexpected ram_high, reason: {}",
            d.reason
        );
    }

    // ------------------------------------------------------------------
    // Rule 3 — large batch → Local
    // ------------------------------------------------------------------

    #[test]
    fn large_batch_routes_to_local() {
        let d = evaluate(&cfg(), snap(10, 512), 100);
        assert_eq!(d.backend, EmbeddingBackend::Local);
        assert!(d.reason.contains("large_batch"), "reason: {}", d.reason);
    }

    #[test]
    fn batch_exactly_at_threshold_routes_to_local() {
        let d = evaluate(&cfg(), snap(10, 512), 64);
        assert_eq!(d.backend, EmbeddingBackend::Local);
        assert!(d.reason.contains("large_batch"), "reason: {}", d.reason);
    }

    #[test]
    fn batch_one_below_threshold_does_not_trigger_rule3() {
        let d = evaluate(&cfg(), snap(10, 512), 63);
        assert!(!d.reason.contains("large_batch"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 4 — default → Local
    // ------------------------------------------------------------------

    #[test]
    fn default_routes_to_local() {
        // All values well within limits, small batch.
        let d = evaluate(&cfg(), snap(10, 512), 1);
        assert_eq!(d.backend, EmbeddingBackend::Local);
        assert!(d.reason.contains("default"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule priority (CPU takes precedence over RAM and batch)
    // ------------------------------------------------------------------

    #[test]
    fn cpu_rule_takes_precedence_over_ram() {
        // Both CPU and RAM are high; CPU rule fires first.
        let d = evaluate(&cfg(), snap(90, 3000), 1);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("cpu_high"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_rule_takes_precedence_over_large_batch() {
        // High CPU + large batch → CPU rule wins → Api.
        let d = evaluate(&cfg(), snap(90, 512), 200);
        assert_eq!(d.backend, EmbeddingBackend::Api);
        assert!(d.reason.contains("cpu_high"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Decision fields are always populated
    // ------------------------------------------------------------------

    #[test]
    fn decision_reason_is_never_empty() {
        for (cpu, ram, batch) in [
            (90u8, 512u64, 1usize),
            (10, 3000, 1),
            (10, 512, 100),
            (10, 512, 1),
        ] {
            let d = evaluate(&cfg(), snap(cpu, ram), batch);
            assert!(
                !d.reason.is_empty(),
                "empty reason for cpu={cpu} ram={ram} batch={batch}"
            );
        }
    }

    // ------------------------------------------------------------------
    // as_str / Display
    // ------------------------------------------------------------------

    #[test]
    fn backend_as_str_local() {
        assert_eq!(EmbeddingBackend::Local.as_str(), "local");
        assert_eq!(format!("{}", EmbeddingBackend::Local), "local");
    }

    #[test]
    fn backend_as_str_api() {
        assert_eq!(EmbeddingBackend::Api.as_str(), "api");
        assert_eq!(format!("{}", EmbeddingBackend::Api), "api");
    }
}
