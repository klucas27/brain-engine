//! Context assembly: pack retrieved chunks into a token budget.
//!
//! Chunks are greedily added in retrieval rank order (most similar first) until
//! the cumulative token count would exceed `budget`.  Partial chunks are not
//! split — a chunk that would exceed the budget is simply skipped, and the next
//! one is tried.

use crate::retrieve::RetrievedChunk;

/// The assembled context ready to inject or display.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Chunks selected within the budget, in rank order.
    pub chunks: Vec<RetrievedChunk>,
    /// Total estimated tokens of all selected chunks.
    ///
    /// This is also the **real cost** added to the prompt — see [`Context::real_cost`].
    pub context_tokens: usize,
    /// Estimated tokens for the *entire* indexed project sent verbatim.
    /// Used only as the denominator for the (non-accumulated) theoretical metrics.
    pub project_tokens: usize,
    /// **Theoretical** reduction vs. a full-project dump: `project_tokens - context_tokens`.
    ///
    /// Informative only. MUST NOT be accumulated across requests — the project size
    /// is a fixed baseline, so summing it produces meaningless inflated totals.
    pub theoretical_saved: usize,
    /// Chunks that existed but were dropped because they exceeded the budget.
    pub dropped_count: usize,
}

impl Context {
    /// The **real cost**: tokens actually injected into the prompt this request.
    ///
    /// This is the metric that should be accumulated and optimised against.
    pub fn real_cost(&self) -> usize {
        self.context_tokens
    }

    /// Efficiency ratio in `[0.0, 1.0]`: how much of the project had to be used
    /// (`context_tokens / project_tokens`). Lower is better. 0.0 if size unknown.
    pub fn efficiency_ratio(&self) -> f32 {
        if self.project_tokens == 0 {
            return 0.0;
        }
        self.context_tokens as f32 / self.project_tokens as f32
    }

    /// Theoretical reduction percentage vs. a full-project dump, rounded to one
    /// decimal. Informative only — never accumulate this.
    pub fn reduction_pct(&self) -> f64 {
        if self.project_tokens == 0 {
            return 0.0;
        }
        let saved = self.theoretical_saved as f64;
        let total = self.project_tokens as f64;
        (saved / total * 1000.0).round() / 10.0
    }
}

/// Assemble chunks into a context that fits within `budget` tokens.
///
/// `project_tokens` is the estimated total tokens of the whole indexed project,
/// used only for the savings metric — pass `0` to disable that metric.
pub fn assemble(retrieved: Vec<RetrievedChunk>, budget: usize, project_tokens: usize) -> Context {
    let mut ctx = Context {
        project_tokens,
        ..Context::default()
    };

    for chunk in retrieved {
        let t = chunk.token_estimate;
        if ctx.context_tokens + t <= budget {
            ctx.context_tokens += t;
            ctx.chunks.push(chunk);
        } else {
            ctx.dropped_count += 1;
        }
    }

    ctx.theoretical_saved = project_tokens.saturating_sub(ctx.context_tokens);
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retrieve::RetrievedChunk;

    fn make_chunk(id: i64, tokens: usize, score: f32) -> RetrievedChunk {
        RetrievedChunk {
            chunk_id: id,
            file_path: format!("file{id}.rs"),
            start_line: 1,
            end_line: tokens * 4,
            content: "x".repeat(tokens * 4),
            score,
            token_estimate: tokens,
        }
    }

    #[test]
    fn empty_input_yields_empty_context() {
        let ctx = assemble(vec![], 1000, 5000);
        assert!(ctx.chunks.is_empty());
        assert_eq!(ctx.context_tokens, 0);
    }

    #[test]
    fn chunks_fit_within_budget() {
        let chunks = vec![make_chunk(1, 300, 0.9), make_chunk(2, 300, 0.8)];
        let ctx = assemble(chunks, 1000, 5000);
        assert_eq!(ctx.chunks.len(), 2);
        assert_eq!(ctx.context_tokens, 600);
        assert_eq!(ctx.dropped_count, 0);
    }

    #[test]
    fn chunk_exceeding_budget_is_dropped() {
        let chunks = vec![
            make_chunk(1, 800, 0.9),
            make_chunk(2, 400, 0.8), // would exceed 1000
            make_chunk(3, 100, 0.7), // fits
        ];
        let ctx = assemble(chunks, 1000, 5000);
        // chunk 1 (800) fits; chunk 2 (400) would take total to 1200 → skip;
        // chunk 3 (100) fits → total 900.
        assert_eq!(ctx.chunks.len(), 2);
        assert_eq!(ctx.context_tokens, 900);
        assert_eq!(ctx.dropped_count, 1);
    }

    #[test]
    fn real_cost_equals_context_tokens() {
        let chunks = vec![make_chunk(1, 100, 0.9)];
        let ctx = assemble(chunks, 1000, 10_000);
        assert_eq!(ctx.context_tokens, 100);
        assert_eq!(ctx.real_cost(), 100);
    }

    #[test]
    fn theoretical_and_reduction_computed_correctly() {
        let chunks = vec![make_chunk(1, 100, 0.9)];
        let ctx = assemble(chunks, 1000, 10_000);
        assert_eq!(ctx.theoretical_saved, 9_900);
        assert_eq!(ctx.reduction_pct(), 99.0);
        // context_tokens / project_tokens = 100 / 10_000 = 0.01
        assert!((ctx.efficiency_ratio() - 0.01).abs() < 1e-6);
    }

    #[test]
    fn zero_project_tokens_metrics_are_zero() {
        let chunks = vec![make_chunk(1, 100, 0.9)];
        let ctx = assemble(chunks, 1000, 0);
        assert_eq!(ctx.reduction_pct(), 0.0);
        assert_eq!(ctx.efficiency_ratio(), 0.0);
    }
}
