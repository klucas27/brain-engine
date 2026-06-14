//! OpenAI-compatible remote embedding backend.
//!
//! Works with any API that follows the OpenAI embeddings spec:
//!   POST <api_base>/embeddings
//!   Authorization: Bearer <key>
//!   {"model": "...", "input": ["text", ...]}
//!
//! Tested providers: DeepSeek, OpenAI. Configure via ~/.brain/providers.json.

use std::env;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::embedder::Embedder;
use crate::error::{EmbedError, EmbedResult};

pub struct ApiEmbedder {
    client: Client,
    url: String,
    model: String,
    api_key: String,
    dim: usize,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

impl ApiEmbedder {
    /// Build from parsed `providers.json` fields.
    ///
    /// `api_key_env` is the name of the env var holding the secret key.
    /// `dim` is the output dimension of the chosen model (e.g. 1024 for
    /// DeepSeek, 1536 for text-embedding-3-small).
    pub fn new(api_base: &str, model: &str, api_key_env: &str, dim: usize) -> EmbedResult<Self> {
        let api_key = env::var(api_key_env)
            .map_err(|_| EmbedError::MissingApiKey(api_key_env.to_string()))?;
        Ok(Self {
            client: Client::new(),
            url: format!("{}/embeddings", api_base.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            dim,
        })
    }
}

impl Embedder for ApiEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        let body = EmbedRequest { model: &self.model, input: texts };

        let resp = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .map_err(|e| EmbedError::Api(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(EmbedError::Api(format!("HTTP {status}: {text}")));
        }

        let mut response: EmbedResponse =
            resp.json().map_err(|e| EmbedError::Api(format!("invalid response JSON: {e}")))?;

        // API spec says index order is not guaranteed — sort before returning.
        response.data.sort_unstable_by_key(|d| d.index);

        Ok(response.data.into_iter().map(|d| d.embedding).collect())
    }
}
