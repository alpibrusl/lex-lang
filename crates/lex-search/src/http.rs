//! HTTP-backed embedder (#224 slice 2).
//!
//! Two wire formats:
//!
//! * **Ollama** — `POST <url>/api/embeddings` with body
//!   `{"model": "...", "prompt": "..."}`. Response is
//!   `{"embedding": [f32, ...]}`. One request per text — Ollama's
//!   classic embeddings endpoint isn't batchable. The `CachingEmbedder`
//!   wrapper makes repeat calls cheap.
//! * **OpenAI-compat** — `POST <url>/v1/embeddings` with body
//!   `{"model": "...", "input": ["t1", "t2", ...]}`. Response is
//!   `{"data": [{"embedding": [...]}, ...]}`. Batch-friendly.
//!
//! The caller picks via `LEX_EMBED_PROVIDER=ollama|openai`. If
//! unset, we default to `ollama` because that's the most common
//! local-LLM workflow.

use crate::embedder::{EmbedError, Embedder};
use serde::{Deserialize, Serialize};

/// Wire-format flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Ollama,
    OpenAi,
}

impl Provider {
    fn endpoint(self, base: &str) -> String {
        let trimmed = base.trim_end_matches('/');
        match self {
            Provider::Ollama => format!("{trimmed}/api/embeddings"),
            Provider::OpenAi => format!("{trimmed}/v1/embeddings"),
        }
    }
}

/// HTTP embedder. The constructor [`HttpEmbedder::from_env`] reads
/// `LEX_EMBED_URL`, `LEX_EMBED_PROVIDER`, `LEX_EMBED_MODEL`,
/// `LEX_EMBED_API_KEY` (the last is OpenAI-only). All but URL have
/// reasonable defaults.
pub struct HttpEmbedder {
    agent: ureq::Agent,
    base_url: String,
    provider: Provider,
    model: String,
    api_key: Option<String>,
    /// Cached vector dimension, learned from the first successful
    /// response. Avoids returning a different `dim()` mid-session
    /// if the model decides to misbehave (defensive — shouldn't
    /// happen with a single fixed model).
    dim_cell: std::sync::OnceLock<usize>,
}

impl std::fmt::Debug for HttpEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpEmbedder")
            .field("base_url", &self.base_url)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &if self.api_key.is_some() { "<set>" } else { "<unset>" })
            .finish()
    }
}

impl HttpEmbedder {
    /// Construct a new embedder for the given URL + provider.
    pub fn new(base_url: impl Into<String>, provider: Provider, model: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .build()
                .into(),
            base_url: base_url.into(),
            provider,
            model: model.into(),
            api_key: None,
            dim_cell: std::sync::OnceLock::new(),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Read configuration from env vars and construct. Returns
    /// `Ok(None)` when `LEX_EMBED_URL` is unset so the caller can
    /// fall back to [`crate::MockEmbedder`].
    pub fn from_env() -> Result<Option<Self>, EmbedError> {
        let url = match std::env::var("LEX_EMBED_URL") {
            Ok(v) if !v.is_empty() => v,
            _ => return Ok(None),
        };
        let provider = match std::env::var("LEX_EMBED_PROVIDER").ok().as_deref() {
            Some("openai") | Some("openai-compat") => Provider::OpenAi,
            Some("ollama") | None | Some("") => Provider::Ollama,
            Some(other) => return Err(EmbedError::Generic(format!(
                "LEX_EMBED_PROVIDER must be `ollama` or `openai`, got `{other}`"
            ))),
        };
        let model = std::env::var("LEX_EMBED_MODEL").ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| match provider {
                Provider::Ollama => "nomic-embed-text".into(),
                Provider::OpenAi => "text-embedding-3-small".into(),
            });
        let api_key = std::env::var("LEX_EMBED_API_KEY").ok().filter(|s| !s.is_empty());
        let mut e = Self::new(url, provider, model);
        if let Some(k) = api_key { e = e.with_api_key(k); }
        Ok(Some(e))
    }

    /// Provider this embedder talks to (test introspection).
    pub fn provider(&self) -> Provider { self.provider }

    pub fn model(&self) -> &str { &self.model }

    fn record_dim(&self, observed: usize) {
        let _ = self.dim_cell.set(observed);
    }

    fn ollama_embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        #[derive(Serialize)]
        struct Req<'a> { model: &'a str, prompt: &'a str }
        #[derive(Deserialize)]
        struct Resp { embedding: Vec<f32> }
        let url = self.provider.endpoint(&self.base_url);
        let req = Req { model: &self.model, prompt: text };
        let resp: Resp = self.agent.post(&url)
            .send_json(&req)
            .map_err(|e| EmbedError::Generic(format!("ollama POST {url}: {e}")))?
            .body_mut().read_json()
            .map_err(|e| EmbedError::Generic(format!("ollama parse: {e}")))?;
        Ok(resp.embedding)
    }

    fn openai_embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        #[derive(Serialize)]
        struct Req<'a> { model: &'a str, input: &'a [&'a str] }
        #[derive(Deserialize)]
        struct DataItem { embedding: Vec<f32> }
        #[derive(Deserialize)]
        struct Resp { data: Vec<DataItem> }
        let url = self.provider.endpoint(&self.base_url);
        let req = Req { model: &self.model, input: texts };
        let mut request = self.agent.post(&url);
        if let Some(k) = &self.api_key {
            request = request.header("authorization", &format!("Bearer {k}"));
        }
        let resp: Resp = request
            .send_json(&req)
            .map_err(|e| EmbedError::Generic(format!("openai POST {url}: {e}")))?
            .body_mut().read_json()
            .map_err(|e| EmbedError::Generic(format!("openai parse: {e}")))?;
        if resp.data.len() != texts.len() {
            return Err(EmbedError::Generic(format!(
                "openai returned {} embeddings for {} inputs",
                resp.data.len(), texts.len(),
            )));
        }
        Ok(resp.data.into_iter().map(|d| d.embedding).collect())
    }
}

impl Embedder for HttpEmbedder {
    fn dim(&self) -> usize {
        // Until the first call lands a real response, we don't know
        // the dim — the model is the source of truth. Most embedding
        // models settle on 384 / 768 / 1024 / 1536 / 3072 dims, but
        // we let the response tell us. Returning 0 here would be
        // wrong (it's a unit-vector dimension) and 1 would mask bugs;
        // 1024 is a reasonable optimistic default that gets corrected
        // on the first observed response.
        *self.dim_cell.get().unwrap_or(&1024)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() { return Ok(Vec::new()); }
        let out: Vec<Vec<f32>> = match self.provider {
            Provider::Ollama => texts.iter()
                .map(|t| self.ollama_embed(t))
                .collect::<Result<_, _>>()?,
            Provider::OpenAi => self.openai_embed_batch(texts)?,
        };
        if let Some(first) = out.first() {
            self.record_dim(first.len());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_endpoint_is_built_from_base() {
        assert_eq!(
            Provider::Ollama.endpoint("http://localhost:11434"),
            "http://localhost:11434/api/embeddings",
        );
        // Trailing slash is tolerated.
        assert_eq!(
            Provider::Ollama.endpoint("http://localhost:11434/"),
            "http://localhost:11434/api/embeddings",
        );
    }

    #[test]
    fn openai_endpoint_is_built_from_base() {
        assert_eq!(
            Provider::OpenAi.endpoint("https://api.openai.com"),
            "https://api.openai.com/v1/embeddings",
        );
    }

    #[test]
    fn from_env_returns_none_without_url() {
        // Test relies on the env var being unset; tests don't share
        // global env so we explicitly remove it.
        // SAFETY: tests in this module are single-threaded with
        // respect to env; not the case for parallel tests in real
        // Rust test runs but this single-process check is fine.
        // (We'd use std::env::set_var via a mutex lock for shared
        // env, but here we simply remove and re-check.)
        unsafe { std::env::remove_var("LEX_EMBED_URL"); }
        assert!(HttpEmbedder::from_env().unwrap().is_none());
    }

    #[test]
    fn from_env_rejects_unknown_provider() {
        unsafe {
            std::env::set_var("LEX_EMBED_URL", "http://example.com");
            std::env::set_var("LEX_EMBED_PROVIDER", "voyage");
        }
        let r = HttpEmbedder::from_env();
        unsafe {
            std::env::remove_var("LEX_EMBED_URL");
            std::env::remove_var("LEX_EMBED_PROVIDER");
        }
        assert!(matches!(r, Err(EmbedError::Generic(_))));
    }

    #[test]
    fn debug_redacts_api_key() {
        let e = HttpEmbedder::new("http://x", Provider::OpenAi, "m")
            .with_api_key("super-secret");
        let s = format!("{e:?}");
        assert!(!s.contains("super-secret"),
            "Debug output must not contain the API key; got: {s}");
        assert!(s.contains("<set>"));
    }
}
