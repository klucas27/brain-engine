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
//! 2.5. **Rate-limit block** — if `~/.brain/llm_state.json` records an active
//!    block for Claude (set by `brain llm block claude` or auto-detected from
//!    the Stop hook) → `DeepSeek` until the block expires.
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
/// * `cfg`               — decision thresholds from the global config.
/// * `snapshot`          — current CPU/RAM metrics (use [`SystemSnapshot::capture`] in
///   production; inject a fixed value in tests).
/// * `forced`            — if `Some`, skip all rules and return this route.
/// * `is_claude_blocked` — `true` when `~/.brain/llm_state.json` records an
///   active rate-limit block for Claude (caller reads the file; keeps this fn pure).
pub fn route(
    cfg: &DecisionConfig,
    snapshot: SystemSnapshot,
    forced: Option<LlmRoute>,
    is_claude_blocked: bool,
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

    // Rule 2.5 — Claude rate-limited → DeepSeek until block expires
    if is_claude_blocked {
        return RouteDecision {
            route: LlmRoute::DeepSeek,
            reason: "rate_limit: Claude is blocked (token quota exhausted) — \
                     using DeepSeek until the window resets"
                .to_string(),
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
/// Read [`crate::llm_state`] before calling this and pass `is_claude_blocked`.
pub fn route_live(
    cfg: &DecisionConfig,
    forced: Option<LlmRoute>,
    is_claude_blocked: bool,
) -> RouteDecision {
    let snapshot = SystemSnapshot::capture();
    route(cfg, snapshot, forced, is_claude_blocked)
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
        DecisionConfig::default()
    }

    fn snap(cpu_pct: u8, ram_used_mb: u64) -> SystemSnapshot {
        SystemSnapshot { cpu_pct, ram_used_mb }
    }

    // ------------------------------------------------------------------
    // Rule 1 — forced override
    // ------------------------------------------------------------------

    #[test]
    fn forced_claude_ignores_load() {
        let d = route(&cfg(), snap(95, 4000), Some(LlmRoute::Claude), false);
        assert_eq!(d.route, LlmRoute::Claude);
        assert!(d.reason.contains("forced"), "reason: {}", d.reason);
    }

    #[test]
    fn forced_deepseek_ignores_load() {
        let d = route(&cfg(), snap(5, 128), Some(LlmRoute::DeepSeek), false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("forced"), "reason: {}", d.reason);
    }

    #[test]
    fn forced_claude_wins_even_when_blocked() {
        // A forced CLI flag overrides even a rate-limit block.
        let d = route(&cfg(), snap(10, 512), Some(LlmRoute::Claude), true);
        assert_eq!(d.route, LlmRoute::Claude);
        assert!(d.reason.contains("forced"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 2 — high CPU / RAM → DeepSeek
    // ------------------------------------------------------------------

    #[test]
    fn high_cpu_routes_to_deepseek() {
        let d = route(&cfg(), snap(90, 512), None, false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("load_high"), "reason: {}", d.reason);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_at_exact_threshold_routes_to_deepseek() {
        let d = route(&cfg(), snap(80, 512), None, false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_one_below_threshold_does_not_route_for_cpu() {
        let d = route(&cfg(), snap(79, 512), None, false);
        assert!(!d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    #[test]
    fn high_ram_routes_to_deepseek() {
        let d = route(&cfg(), snap(10, 3000), None, false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("ram"), "reason: {}", d.reason);
    }

    #[test]
    fn ram_at_exact_threshold_routes_to_deepseek() {
        let d = route(&cfg(), snap(10, 2048), None, false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("ram"), "reason: {}", d.reason);
    }

    #[test]
    fn cpu_rule_fires_before_ram_rule() {
        let d = route(&cfg(), snap(90, 3000), None, false);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 2.5 — rate-limit block
    // ------------------------------------------------------------------

    #[test]
    fn rate_limit_block_switches_to_deepseek() {
        let d = route(&cfg(), snap(10, 512), None, true);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("rate_limit"), "reason: {}", d.reason);
    }

    #[test]
    fn no_block_uses_claude() {
        let d = route(&cfg(), snap(10, 512), None, false);
        assert_eq!(d.route, LlmRoute::Claude);
    }

    #[test]
    fn cpu_high_fires_before_rate_limit_block() {
        // Rule 2 (CPU) has higher priority than Rule 2.5 (block).
        // Both route to DeepSeek, but the reason should mention cpu.
        let d = route(&cfg(), snap(90, 512), None, true);
        assert_eq!(d.route, LlmRoute::DeepSeek);
        assert!(d.reason.contains("cpu"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Rule 3 — default → Claude
    // ------------------------------------------------------------------

    #[test]
    fn default_routes_to_claude() {
        let d = route(&cfg(), snap(10, 512), None, false);
        assert_eq!(d.route, LlmRoute::Claude);
        assert!(d.reason.contains("default"), "reason: {}", d.reason);
    }

    // ------------------------------------------------------------------
    // Reason is always non-empty
    // ------------------------------------------------------------------

    #[test]
    fn route_decision_reason_never_empty() {
        for (cpu, ram, forced, blocked) in [
            (90u8, 512u64, None, false),
            (10, 3000, None, false),
            (10, 512, None, false),
            (10, 512, None, true),
            (10, 512, Some(LlmRoute::Claude), false),
            (10, 512, Some(LlmRoute::DeepSeek), false),
        ] {
            let d = route(&cfg(), snap(cpu, ram), forced, blocked);
            assert!(
                !d.reason.is_empty(),
                "empty reason for cpu={cpu} ram={ram} forced={forced:?} blocked={blocked}"
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

