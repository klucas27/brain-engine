//! The `Embedder` trait — the single abstraction over all embedding backends.
//!
//! Every provider (local ONNX, DeepSeek API, OpenAI API, …) implements this
//! trait.  The CLI and future daemon hold a `Box<dyn Embedder>` and never
//! depend on the concrete type.

use crate::error::EmbedResult;

/// A stateless (or internally-locked) embedding provider.
///
/// Implementations must be `Send + Sync` so they can be shared across the
/// Rayon thread-pool (Phase 2) and the async daemon (Phase 7).
pub trait Embedder: Send + Sync {
    /// Stable identifier for the model, stored in the `meta` table to detect
    /// provider changes.  Example: `"BAAI/bge-small-en-v1.5"`.
    fn model_id(&self) -> &str;

    /// Output vector length.  Mixing dimensions corrupts ANN search, so a
    /// change here forces a full reindex.
    fn dim(&self) -> usize;

    /// Embed a batch of texts.  Returns one `Vec<f32>` of length `self.dim()`
    /// per input text, in the same order.
    fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>>;
}
