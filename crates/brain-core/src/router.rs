//! AI router — LLM backend selector (DeepSeek vs Claude).
//!
//! The router decides which LLM to send a request to based on the same system
//! snapshot used by the embedding decision engine.  Routing logic is kept
//! intentionally simple and fully deterministic so it can be tested without
//! touching real APIs.
//!
//! # Rules (evaluated in priority order)
//!
//! 1. **Forced override** — if `forced` is `Some(route)`, return it unchanged.
//!    Useful for `--llm deepseek` / `--llm claude` CLI flags.
//! 2. **High system load** — if CPU% or RAM is above threshold → `DeepSeek`
//!    (lighter local footprint while the machine is stressed).
//! 3. **Default** — `Claude` (via Claude Code, zero additional API cost).
//!
//! # Separation of concerns
//!
//! Embedding routing (`decision.rs`) and LLM routing (`router.rs`) are kept in
//! separate modules because:
//! * They have different cost/privacy trade-offs.
//! * Embedding decisions are batch-size-sensitive; LLM decisions are not.
//! * Each can evolve independently.

use crate::config::DecisionConfig;
use crate::decision::SystemSnapshot;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which LLM backend the router selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRoute {
    /// Route the request through Claude (via Claude Code — no direct API call).
    Claude,
    /// Route the request to DeepSeek's chat API.
    DeepSeek,
}

impl LlmRoute {
    /// Returns `"claude"` or `"deepseek"` — matches the keys in `providers.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            LlmRoute::Claude => "claude",
            LlmRoute::DeepSeek => "deepseek",
        }
    }
}

impl std::fmt::Display for LlmRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of LLM routing: the chosen backend plus the reason.
#[derive(Debug, Clone)]
pub struct RouteDecision {
    /// The selected LLM backend.
    pub route: LlmRoute,
    /// A short, human-readable explanation of the rule that fired.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

/// Route a single LLM request given the system state.
///
/// # Parameters
/// * `cfg`      — decision thresholds from the global config.
/// * `snapshot` — current CPU/RAM metrics (use [`SystemSnapshot::capture`] in
///   production; inject a fixed value in tests).
/// * `forced`   — if `Some`, skip all rules and return this route with an
///   explanatory reason.
pub fn route(
    cfg: &DecisionConfig,
    snapshot: SystemSnapshot,
    forced: Option<LlmRoute>,
) -> RouteDecision {
    // Rule 1 — forced override (e.g. --llm flag)
    if let Some(r) = forced {
        return RouteDecision {
            route: r,
            reason: format!("forced: caller explicitly requested '{}'", r.as_str()),
        };
    }

    // Rule 2 — high system load → DeepSeek (lower local footprint)
    if snapshot.cpu_pct >= cfg.cpu_high_threshold {
        return RouteDecision {
            route: LlmRoute::DeepSeek,
            reason: format!(
                "load_high: cpu {}% >= threshold {}% — routing to DeepSeek to reduce local load",
                snapshot.cpu_pct, cfg.cpu_high_threshold
            ),
        };
    }

    if snapshot.ram_used_mb >= cfg.memory_high_threshold_mb {
        return RouteDecision {
            route: LlmRoute::DeepSeek,
            reason: format!(
                "load_high: ram {}MB >= threshold {}MB — routing to DeepSeek to reduce local load",
                snapshot.ram_used_mb, cfg.memory_high_threshold_mb
            ),
        };
    }

    // Rule 3 — default → Claude (via Claude Code, free and private)
    RouteDecision {
        route: LlmRoute::Claude,
        reason: "default: system within limits, using Claude via Claude Code".to_string(),
    }
}

/// Sample live system metrics and route an LLM request.
///
/// Prefer [`route`] in tests; use this in production code paths.
pub fn route_live(cfg: &DecisionConfig, forced: Option<LlmRoute>) -> RouteDecision {
    let snapshot = SystemSnapshot::capture();
    route(cfg, snapshot, forced)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DecisionConfig;
    use crate::decision::SystemSnapshot;

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
    // Rule 1 — forced override
    // ------------------------------------------------------------------

    #[test]
    fn forced_claude_ignores_load() {
        // Even under heavy load, the forced route wins.
        let d = route(&cfg(), snap(95, 4000), Some(LlmRoute::Claude));
        assert_eq!(d.route, LlmRoute::Claude);
        assert!(d.reason.contains("forced"), "reason: {}", d.reason);
    }

    #[test]
    fn forced_deepseek_ignores_load() {
        // Even under no load, the forced route wins.
        let d = route(&cfg(), snap(5, 128), Some(LlmRoute::DeepSeek));
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("forced"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 2 — high CPU → DeepSeek
    // ------------------------------------------------------------------

    #[test]
    fn high_cpu_routes_to_deepseek() {
        let d = route(&cfg(), snap(90, 512), None);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("load_high"), "reason: {}", d.reason);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_at_exact_threshold_routes_to_deepseek() {
        let d = route(&cfg(), snap(80, 512), None);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_one_below_threshold_does_not_route_to_deepseek_for_cpu() {
        let d = route(&cfg(), snap(79, 512), None);
        assert!(
            !d.reason.contains("cpu"),
            "should not mention cpu, reason: {}",
            d.reason
        );
    }

    // ------------------------------------------------------------------
    // Rule 2 — high RAM → DeepSeek
    // ------------------------------------------------------------------

    #[test]
    fn high_ram_routes_to_deepseek() {
        let d = route(&cfg(), snap(10, 3000), None);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("ram"), "reason: {}", d.reason);
    }

    #[test]
    fn ram_at_exact_threshold_routes_to_deepseek() {
        let d = route(&cfg(), snap(10, 2048), None);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("ram"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 3 — default → Claude
    // ------------------------------------------------------------------

    #[test]
    fn default_routes_to_claude() {
        let d = route(&cfg(), snap(10, 512), None);
        assert_eq!(d.route, LlmRoute::Claude);
        assert!(d.reason.contains("default"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Priority: CPU rule fires before RAM rule
    // ------------------------------------------------------------------

    #[test]
    fn cpu_rule_fires_before_ram_rule() {
        let d = route(&cfg(), snap(90, 3000), None);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        // Reason should mention cpu, not ram, because CPU check comes first.
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Decision fields are always populated
    // ------------------------------------------------------------------

    #[test]
    fn route_decision_reason_never_empty() {
        for (cpu, ram, forced) in [
            (90u8, 512u64, None),
            (10, 3000, None),
            (10, 512, None),
            (10, 512, Some(LlmRoute::Claude)),
            (10, 512, Some(LlmRoute::DeepSeek)),
        ] {
            let d = route(&cfg(), snap(cpu, ram), forced);
            assert!(
                !d.reason.is_empty(),
                "empty reason for cpu={cpu} ram={ram} forced={forced:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // as_str / Display
    // ------------------------------------------------------------------

    #[test]
    fn llm_route_as_str_claude() {
        assert_eq!(LlmRoute::Claude.as_str(), "claude");
        assert_eq!(format!("{}", LlmRoute::Claude), "claude");
    }

    #[test]
    fn llm_route_as_str_deepseek() {
        assert_eq!(LlmRoute::DeepSeek.as_str(), "deepseek");
        assert_eq!(format!("{}", LlmRoute::DeepSeek), "deepseek");
    }
}
