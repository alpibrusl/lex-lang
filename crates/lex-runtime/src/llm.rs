//! HTTP-backed LLM completions for the `[llm_local]` and
//! `[llm_cloud]` effects (#196).
//!
//! Configuration is via environment variables — the simplest
//! shape that doesn't pull a config file format into the
//! runtime. Power-user override is the existing
//! `lex_bytecode::vm::EffectHandler` trait: callers that want
//! something more elaborate (custom auth, batching, fallback
//! providers, or non-HTTP transports) wrap `DefaultHandler`
//! and intercept the `agent.local_complete` /
//! `agent.cloud_complete` dispatch.
//!
//! ## `[llm_local]`
//!
//! Defaults to Ollama at `http://localhost:11434`, model
//! `llama3`.
//!
//! - `OLLAMA_HOST` — base URL of the Ollama server.
//! - `LEX_LLM_LOCAL_MODEL` — model name passed to
//!   `/api/generate`.
//!
//! Any service that speaks Ollama's `/api/generate` JSON also
//! works (llama.cpp's compatible mode, vLLM with the right
//! adapter, etc.).
//!
//! ## `[llm_cloud]`
//!
//! Defaults to OpenAI's `/v1/chat/completions`, model
//! `gpt-4o-mini`. The shape is the OpenAI Chat Completions
//! protocol, which **most cloud LLM providers speak natively
//! today** — the env vars below let you point at any of them:
//!
//! - `LEX_LLM_CLOUD_API_KEY` — bearer token (preferred). Falls
//!   back to `OPENAI_API_KEY` if unset, so existing
//!   OpenAI-targeted setups keep working unchanged.
//! - `LEX_LLM_CLOUD_BASE_URL` / `OPENAI_BASE_URL` — endpoint
//!   prefix (the `/chat/completions` is appended). Default is
//!   `https://api.openai.com/v1`.
//! - `LEX_LLM_CLOUD_MODEL` — model name.
//!
//! Provider matrix (concrete env-var combinations):
//!
//! | Provider | `LEX_LLM_CLOUD_BASE_URL` | `LEX_LLM_CLOUD_MODEL` |
//! |---|---|---|
//! | OpenAI | (default) | `gpt-4o-mini`, `gpt-4o`, `o1-mini`, … |
//! | Mistral | `https://api.mistral.ai/v1` | `mistral-large-latest`, `mistral-small-latest`, … |
//! | Together AI | `https://api.together.xyz/v1` | model id from their catalog |
//! | Groq | `https://api.groq.com/openai/v1` | `llama-3.1-70b-versatile`, … |
//! | DeepSeek | `https://api.deepseek.com/v1` | `deepseek-chat`, … |
//! | vLLM (self-hosted) | `http://your-vllm:8000/v1` | the model the vLLM is serving |
//! | Anthropic | use a translating proxy (e.g. `litellm`) | claude model id |
//!
//! Anthropic specifically doesn't ship native chat-completions
//! today; pair it with a proxy like `litellm` or a custom
//! `EffectHandler` impl.
//!
//! ## Replay determinism
//!
//! Not guaranteed today. Either provider may return different
//! completions for the same prompt across runs. Wrap the
//! handler if you need replay fidelity (pin a seed, snapshot
//! the model hash, etc.) — soft-agent's audit-replay pipeline
//! (#187) is where that lives.

use serde_json::{json, Value as JsonValue};
use std::time::Duration;

const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";
const DEFAULT_LOCAL_MODEL: &str = "llama3";
const DEFAULT_OPENAI_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_CLOUD_MODEL: &str = "gpt-4o-mini";
const HTTP_TIMEOUT_SECS: u64 = 120;

/// Resolve the Ollama HTTP endpoint and model from env vars.
/// Pure: no I/O, just env reads.
fn local_config() -> (String, String) {
    let host = std::env::var("OLLAMA_HOST")
        .unwrap_or_else(|_| DEFAULT_OLLAMA_HOST.to_string());
    let model = std::env::var("LEX_LLM_LOCAL_MODEL")
        .unwrap_or_else(|_| DEFAULT_LOCAL_MODEL.to_string());
    (host, model)
}

/// Resolve the chat-completions endpoint, model, and api key
/// from env vars. The api key is required for the cloud effect
/// to dispatch at all; absence surfaces as an `Err` to the Lex
/// caller rather than a silent fallback.
///
/// Lookup order, falling through on missing/empty:
///
/// - api key: `LEX_LLM_CLOUD_API_KEY` (preferred) → `OPENAI_API_KEY`
/// - base url: `LEX_LLM_CLOUD_BASE_URL` (preferred) → `OPENAI_BASE_URL`
///   → default OpenAI endpoint
/// - model: `LEX_LLM_CLOUD_MODEL` → default `gpt-4o-mini`
///
/// The `OPENAI_*` fallbacks let existing setups keep working
/// without changes; the `LEX_LLM_CLOUD_*` names are the recommended
/// spelling for new deployments since the API shape is shared
/// across many non-OpenAI providers (Mistral, Groq, Together,
/// DeepSeek, vLLM, …).
fn cloud_config() -> Result<(String, String, String), String> {
    let key = pick_env(&["LEX_LLM_CLOUD_API_KEY", "OPENAI_API_KEY"])
        .ok_or_else(||
            "agent.cloud_complete: neither LEX_LLM_CLOUD_API_KEY nor OPENAI_API_KEY env var set"
                .to_string())?;
    let base = pick_env(&["LEX_LLM_CLOUD_BASE_URL", "OPENAI_BASE_URL"])
        .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string());
    let model = std::env::var("LEX_LLM_CLOUD_MODEL")
        .unwrap_or_else(|_| DEFAULT_CLOUD_MODEL.to_string());
    Ok((base, model, key))
}

/// First non-empty env var from `names`, or `None`.
fn pick_env(names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            if !v.is_empty() { return Some(v); }
        }
    }
    None
}

/// Build the JSON body for an Ollama `/api/generate` request.
/// Factored so unit tests can pin the shape without an HTTP
/// round-trip.
pub(crate) fn ollama_request_body(model: &str, prompt: &str) -> JsonValue {
    json!({
        "model": model,
        "prompt": prompt,
        // Disable streaming — Ollama's default chunked response
        // format is harder to consume and the synchronous one
        // returns the full text in `.response`.
        "stream": false,
    })
}

/// Build the JSON body for an OpenAI `/chat/completions`
/// request. Single-turn: the prompt becomes a `user` message.
/// Multi-turn (system + history) support is left to the
/// EffectHandler escape-hatch.
pub(crate) fn openai_request_body(model: &str, prompt: &str) -> JsonValue {
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
    })
}

/// Extract the completion text from an Ollama response. Ollama
/// returns `{"model":"...", "response":"...", ...}`.
fn ollama_extract(resp: &JsonValue) -> Result<String, String> {
    resp.get("response")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!(
            "ollama: response missing `response` field: {}",
            resp.to_string().chars().take(200).collect::<String>()
        ))
}

/// Extract the assistant message from an OpenAI chat-completion
/// response. Path is `choices[0].message.content`.
fn openai_extract(resp: &JsonValue) -> Result<String, String> {
    resp.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| format!(
            "openai: response missing choices[0].message.content: {}",
            resp.to_string().chars().take(200).collect::<String>()
        ))
}

/// Build a configured ureq agent with the global timeout that
/// the LLM endpoints use. Factored out of `local_complete` /
/// `cloud_complete` so the two share the same timeout policy.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(HTTP_TIMEOUT_SECS)))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

fn read_body_json(mut resp: ureq::http::Response<ureq::Body>) -> Result<JsonValue, String> {
    let bytes = resp.body_mut().read_to_vec()
        .map_err(|e| format!("read response body: {e}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse response JSON: {e}"))
}

/// Run a completion against Ollama. Synchronous; respects
/// `[llm_local]` policy (the caller has already gated).
pub fn local_complete(prompt: &str) -> Result<String, String> {
    let (host, model) = local_config();
    let url = format!("{}/api/generate", host.trim_end_matches('/'));
    let body = serde_json::to_vec(&ollama_request_body(&model, prompt))
        .map_err(|e| format!("serialize ollama request: {e}"))?;
    let resp = http_agent().post(&url)
        .header("content-type", "application/json")
        .send(&body[..])
        .map_err(|e| format!("ollama POST {url}: {e}"))?;
    let json = read_body_json(resp).map_err(|e| format!("ollama: {e}"))?;
    ollama_extract(&json)
}

/// Run a completion against OpenAI's chat-completions API.
/// Synchronous; respects `[llm_cloud]` policy.
pub fn cloud_complete(prompt: &str) -> Result<String, String> {
    let (base, model, key) = cloud_config()?;
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let body = serde_json::to_vec(&openai_request_body(&model, prompt))
        .map_err(|e| format!("serialize cloud request: {e}"))?;
    let resp = http_agent().post(&url)
        .header("content-type", "application/json")
        .header("Authorization", &format!("Bearer {key}"))
        .send(&body[..])
        .map_err(|e| format!("cloud POST {url}: {e}"))?;
    let json = read_body_json(resp).map_err(|e| format!("cloud: {e}"))?;
    openai_extract(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_body_is_non_streaming() {
        let b = ollama_request_body("llama3", "hello");
        assert_eq!(b["model"], "llama3");
        assert_eq!(b["prompt"], "hello");
        assert_eq!(b["stream"], false);
    }

    #[test]
    fn openai_body_uses_user_role() {
        let b = openai_request_body("gpt-4o-mini", "hello");
        assert_eq!(b["model"], "gpt-4o-mini");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"], "hello");
    }

    #[test]
    fn ollama_extract_pulls_response_field() {
        let r = json!({"model": "llama3", "response": "hi back", "done": true});
        assert_eq!(ollama_extract(&r).unwrap(), "hi back");
    }

    #[test]
    fn ollama_extract_errors_on_missing_field() {
        let r = json!({"error": "model not found"});
        let e = ollama_extract(&r).unwrap_err();
        assert!(e.contains("missing `response`"));
    }

    #[test]
    fn openai_extract_pulls_choices_zero_message_content() {
        let r = json!({
            "id": "x",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi back" },
                "finish_reason": "stop"
            }]
        });
        assert_eq!(openai_extract(&r).unwrap(), "hi back");
    }

    #[test]
    fn openai_extract_errors_on_missing_path() {
        let r = json!({"error": {"message": "invalid api key"}});
        let e = openai_extract(&r).unwrap_err();
        assert!(e.contains("missing"));
    }

    #[test]
    fn cloud_config_fails_without_api_key() {
        // Note: this mutates process-global state. Other tests in
        // this module read these env vars too — keep the snapshot/
        // restore pattern uniform so suite-level parallelism stays
        // safe.
        let prior_lex = std::env::var("LEX_LLM_CLOUD_API_KEY").ok();
        let prior_oai = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("LEX_LLM_CLOUD_API_KEY");
        std::env::remove_var("OPENAI_API_KEY");
        let r = cloud_config();
        if let Some(v) = prior_lex { std::env::set_var("LEX_LLM_CLOUD_API_KEY", v); }
        if let Some(v) = prior_oai { std::env::set_var("OPENAI_API_KEY", v); }
        let e = r.unwrap_err();
        assert!(e.contains("LEX_LLM_CLOUD_API_KEY"));
    }

    #[test]
    fn cloud_config_prefers_lex_prefix_then_falls_back_to_openai() {
        let prior_lex_key = std::env::var("LEX_LLM_CLOUD_API_KEY").ok();
        let prior_lex_url = std::env::var("LEX_LLM_CLOUD_BASE_URL").ok();
        let prior_oai_key = std::env::var("OPENAI_API_KEY").ok();
        let prior_oai_url = std::env::var("OPENAI_BASE_URL").ok();
        std::env::set_var("LEX_LLM_CLOUD_API_KEY", "k-lex");
        std::env::set_var("OPENAI_API_KEY", "k-openai");
        std::env::set_var("LEX_LLM_CLOUD_BASE_URL", "https://api.mistral.ai/v1");
        std::env::remove_var("OPENAI_BASE_URL");
        let (base, _model, key) = cloud_config().unwrap();
        // Restore before assertions so a panic doesn't leak state
        // into other tests.
        let restore = |name: &str, v: Option<String>| match v {
            Some(s) => std::env::set_var(name, s),
            None => std::env::remove_var(name),
        };
        restore("LEX_LLM_CLOUD_API_KEY", prior_lex_key);
        restore("LEX_LLM_CLOUD_BASE_URL", prior_lex_url);
        restore("OPENAI_API_KEY", prior_oai_key);
        restore("OPENAI_BASE_URL", prior_oai_url);
        assert_eq!(key, "k-lex");
        assert_eq!(base, "https://api.mistral.ai/v1");
    }

    #[test]
    fn local_config_uses_defaults_without_env() {
        let prior_h = std::env::var("OLLAMA_HOST").ok();
        let prior_m = std::env::var("LEX_LLM_LOCAL_MODEL").ok();
        std::env::remove_var("OLLAMA_HOST");
        std::env::remove_var("LEX_LLM_LOCAL_MODEL");
        let (host, model) = local_config();
        if let Some(v) = prior_h { std::env::set_var("OLLAMA_HOST", v); }
        if let Some(v) = prior_m { std::env::set_var("LEX_LLM_LOCAL_MODEL", v); }
        assert_eq!(host, DEFAULT_OLLAMA_HOST);
        assert_eq!(model, DEFAULT_LOCAL_MODEL);
    }
}
