//! JSON-line protocol shared between daemon and clients.
//!
//! Every message is a single UTF-8 line terminated by `\n`.
//!
//! **Request** (client → daemon):
//! ```json
//! {"id":1,"method":"query","params":{"query":"...","top_k":5,"tokens":4000,"no_cache":false}}
//! ```
//!
//! **Response** (daemon → client):
//! ```json
//! {"id":1,"ok":true,"result":{...}}
//! {"id":1,"ok":false,"error":"..."}
//! ```
//!
//! Supported methods: `ping`, `status`, `query`, `index`, `store`, `symbols`.

use serde::{Deserialize, Serialize};

/// Envelope for every inbound request.
#[derive(Debug, Deserialize)]
pub struct RequestEnvelope {
    /// Caller-chosen correlation id, echoed back in the response.
    pub id: u32,
    /// Method name: `ping` | `status` | `query` | `index` | `store`.
    pub method: String,
    /// Method-specific parameters (may be absent or `null`).
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Envelope for every outbound response.
#[derive(Debug, Serialize)]
pub struct ResponseEnvelope {
    /// Matches the `id` from the corresponding request.
    pub id: u32,
    /// `true` on success, `false` on error.
    pub ok: bool,
    /// Present when `ok = true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Present when `ok = false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ResponseEnvelope {
    pub fn ok(id: u32, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u32, msg: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(msg.into()),
        }
    }
}

/// Parsed parameters for `query`.
#[derive(Debug, Deserialize)]
pub struct QueryParams {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    #[serde(default = "default_tokens")]
    pub tokens: usize,
    #[serde(default)]
    pub no_cache: bool,
}

/// Parsed parameters for `index`.
#[derive(Debug, Deserialize, Default)]
pub struct IndexParams {
    #[serde(default)]
    pub reindex: bool,
    #[serde(default)]
    pub no_embed: bool,
}

/// Parsed parameters for `store` — caches an assistant response under a prompt.
#[derive(Debug, Deserialize)]
pub struct StoreParams {
    /// The user prompt that produced the response (becomes the cache key).
    pub query: String,
    /// The assistant response text to cache verbatim.
    pub response: String,
}

/// Parsed parameters for `symbols`.
#[derive(Debug, Deserialize, Default)]
pub struct SymbolsParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default = "default_symbol_limit")]
    pub limit: usize,
}

fn default_top_k() -> usize {
    5
}
fn default_tokens() -> usize {
    4000
}

fn default_symbol_limit() -> usize {
    20
}
