//! # brain-embed
//!
//! Embedding backends for the Brain Engine.
//!
//! * [`Embedder`]       — the pluggable embedding trait
//! * [`LocalEmbedder`]  — local ONNX backend via fastembed (bge-small-en-v1.5)
//! * [`from_provider`]  — factory: pick a backend from `providers.json` config

pub mod api;
pub mod embedder;
pub mod error;
pub mod local;
pub mod provider;

pub use api::ApiEmbedder;
pub use embedder::Embedder;
pub use error::{EmbedError, EmbedResult};
pub use local::LocalEmbedder;
pub use provider::from_provider;
