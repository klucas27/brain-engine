//! Error type for the embedding subsystem.

pub type EmbedResult<T> = Result<T, EmbedError>;

/// All errors that can originate from embedding operations.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("fastembed error: {0}")]
    Fastembed(String),

    #[error("unknown embedding provider '{0}'; check ~/.brain/providers.json")]
    UnknownProvider(String),

    #[error("unsupported embedding runtime '{0}'")]
    UnsupportedRuntime(String),

    #[error("missing API key: environment variable '{0}' is not set")]
    MissingApiKey(String),

    #[error("API request failed: {0}")]
    Api(String),

    /// Raised when the model pinned in the SQLite `meta` table differs from
    /// the one specified in the current provider config.  The caller decides
    /// whether to fail or trigger a full reindex.
    #[error(
        "embedding model mismatch: index uses '{pinned}' (dim {pinned_dim}) \
         but current config specifies '{current}' (dim {current_dim})"
    )]
    ModelMismatch {
        pinned: String,
        pinned_dim: usize,
        current: String,
        current_dim: usize,
    },
}
