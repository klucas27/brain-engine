//! Local ONNX embedding backend using fastembed + bge-small-en-v1.5.
//!
//! On first use the model is downloaded from HuggingFace Hub (~67 MB) and
//! cached in the directory supplied to [`LocalEmbedder::new`].  Subsequent
//! runs load from cache with no network access.
//!
//! The model produces 384-dimensional L2-normalised embeddings.

use std::path::Path;
use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::embedder::Embedder;
use crate::error::{EmbedError, EmbedResult};

/// HuggingFace model id stored in the SQLite `meta` table.
pub const MODEL_ID: &str = "BAAI/bge-small-en-v1.5";
/// Output embedding dimension for this model.
pub const DIM: usize = 384;

/// Local ONNX embedding backend.
///
/// Wraps `TextEmbedding` in a `Mutex` so the struct is `Sync` even if the
/// underlying ORT session only guarantees `Send`.  The mutex is only
/// contended when multiple threads embed concurrently, which does not happen
/// in Phase 3 (single-threaded CLI).
pub struct LocalEmbedder {
    model: Mutex<TextEmbedding>,
}

impl LocalEmbedder {
    /// Initialise the local embedder.
    ///
    /// `cache_dir` controls where the downloaded model files are stored.
    /// Pass `None` to use fastembed's built-in default
    /// (`~/.cache/huggingface/hub/`).
    pub fn new(cache_dir: Option<&Path>) -> EmbedResult<Self> {
        let mut opts =
            InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(true);
        if let Some(dir) = cache_dir {
            opts = opts.with_cache_dir(dir.to_path_buf());
        }
        let model =
            TextEmbedding::try_new(opts).map_err(|e| EmbedError::Fastembed(e.to_string()))?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

impl Embedder for LocalEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        let guard = self.model.lock().expect("embedder mutex poisoned");
        let owned: Vec<String> = texts.iter().map(|s| s.to_string()).collect();
        guard
            .embed(owned, None)
            .map_err(|e| EmbedError::Fastembed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_id_and_dim_constants() {
        assert_eq!(MODEL_ID, "BAAI/bge-small-en-v1.5");
        assert_eq!(DIM, 384);
    }

    /// Integration test: downloads the real model — requires internet.
    /// Run with: cargo test -- --include-ignored local_embed_smoke
    #[test]
    #[ignore = "requires network access for model download"]
    fn local_embed_smoke() {
        let embedder = LocalEmbedder::new(None).expect("embedder should init");
        assert_eq!(embedder.model_id(), MODEL_ID);
        assert_eq!(embedder.dim(), DIM);

        let result = embedder
            .embed(&["hello world", "rust is fast"])
            .expect("embed should succeed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), DIM);
        assert_eq!(result[1].len(), DIM);
    }
}
