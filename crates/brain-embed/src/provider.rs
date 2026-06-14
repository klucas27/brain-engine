//! Factory: construct the right [`Embedder`] from a `providers.json` entry.
//!
//! The provider name (e.g. `"local"`, `"deepseek"`) comes from the project's
//! `embedding_provider` config field.  The factory reads the matching entry in
//! `~/.brain/providers.json` and instantiates the appropriate backend.

use std::path::Path;

use brain_core::config::Providers;

use crate::embedder::Embedder;
use crate::error::{EmbedError, EmbedResult};
use crate::local::LocalEmbedder;

/// Build an [`Embedder`] from a named provider entry.
///
/// `provider_name` must exist as a key under `providers.embedding`.
/// `model_cache_dir` is forwarded to the local backend for model storage;
/// pass `None` to use the default cache location.
pub fn from_provider(
    provider_name: &str,
    providers: &Providers,
    model_cache_dir: Option<&Path>,
) -> EmbedResult<Box<dyn Embedder>> {
    let cfg = providers
        .embedding
        .get(provider_name)
        .ok_or_else(|| EmbedError::UnknownProvider(provider_name.to_string()))?;

    // The `runtime` field distinguishes local ONNX from remote API calls.
    let runtime = cfg.get("runtime").and_then(|v| v.as_str()).unwrap_or("cpu");

    match runtime {
        "cpu" | "local" => Ok(Box::new(LocalEmbedder::new(model_cache_dir)?)),
        other => Err(EmbedError::UnsupportedRuntime(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use brain_core::config::Providers;

    use crate::provider::from_provider;

    #[test]
    fn unknown_provider_errors() {
        let providers = Providers::default();
        let result = from_provider("nonexistent", &providers, None);
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("nonexistent"), "error: {msg}");
    }

    #[test]
    fn unsupported_runtime_errors() {
        use serde_json::json;
        let mut providers = Providers::default();
        providers.embedding.insert(
            "gpu-provider".to_string(),
            json!({ "runtime": "cuda", "model": "some-model" }),
        );
        let result = from_provider("gpu-provider", &providers, None);
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("cuda"), "error: {msg}");
    }

    /// Integration: actually build the local embedder — requires network.
    #[test]
    #[ignore = "requires network access for model download"]
    fn local_provider_builds() {
        let providers = Providers::default();
        let embedder =
            from_provider("local", &providers, None).expect("should build local embedder");
        assert_eq!(embedder.model_id(), "BAAI/bge-small-en-v1.5");
        assert_eq!(embedder.dim(), 384);
    }
}
