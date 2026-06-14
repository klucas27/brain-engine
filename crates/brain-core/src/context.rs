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
    pub context_tokens: usize,
    /// Estimated tokens for the *entire* indexed project sent verbatim.
    /// Used as the denominator for the savings metric.
    pub project_tokens: usize,
    /// `project_tokens - context_tokens`; always ≥ 0.
    pub tokens_saved: usize,
    /// Chunks that existed but were dropped because they exceeded the budget.
    pub dropped_count: usize,
}

impl Context {
    /// Percentage of project tokens saved, rounded to one decimal.
    pub fn savings_pct(&self) -> f64 {
        if self.project_tokens == 0 {
            return 0.0;
        }
        let saved = self.tokens_saved as f64;
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

    ctx.tokens_saved = project_tokens.saturating_sub(ctx.context_tokens);
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
    fn savings_percentage_computed_correctly() {
        let chunks = vec![make_chunk(1, 100, 0.9)];
        let ctx = assemble(chunks, 1000, 10_000);
        assert_eq!(ctx.context_tokens, 100);
        assert_eq!(ctx.tokens_saved, 9_900);
        assert_eq!(ctx.savings_pct(), 99.0);
    }

    #[test]
    fn zero_project_tokens_savings_pct_is_zero() {
        let chunks = vec![make_chunk(1, 100, 0.9)];
        let ctx = assemble(chunks, 1000, 0);
        assert_eq!(ctx.savings_pct(), 0.0);
    }
}
