//! Integration tests for [`HttpEmbedder`] using a tiny in-process
//! HTTP server. The server returns deterministic vectors derived
//! from the input length so tests can assert exact output without
//! depending on a real embedding model.

use lex_search::{Embedder, HttpEmbedder, Provider};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Spin up a `tiny_http` server on a random port and return
/// (base_url, request_count, shutdown_handle). Each test gets its
/// own server so concurrent runs don't collide.
struct TestServer {
    port: u16,
    requests: Arc<AtomicUsize>,
    _thread: std::thread::JoinHandle<()>,
}

impl TestServer {
    fn start_ollama() -> Self {
        Self::start(|req: &mut tiny_http::Request| {
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).ok();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let prompt = json["prompt"].as_str().unwrap_or("");
            // Deterministic 4-dim vector: [len, len*2, len*3, 1.0]
            // Useful: empty string => [0,0,0,1] (still distinguishable).
            let len = prompt.len() as f32;
            let vec = vec![len, len * 2.0, len * 3.0, 1.0];
            let resp_body = serde_json::json!({"embedding": vec}).to_string();
            tiny_http::Response::from_string(resp_body)
                .with_header("content-type: application/json".parse::<tiny_http::Header>().unwrap())
        })
    }

    fn start_openai() -> Self {
        Self::start(|req: &mut tiny_http::Request| {
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).ok();
            let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let inputs = json["input"].as_array().cloned().unwrap_or_default();
            let data: Vec<serde_json::Value> = inputs.iter().map(|t| {
                let s = t.as_str().unwrap_or("");
                let len = s.len() as f32;
                let vec = vec![len, len * 2.0, len * 3.0, 1.0];
                serde_json::json!({"embedding": vec})
            }).collect();
            let resp_body = serde_json::json!({"data": data}).to_string();
            tiny_http::Response::from_string(resp_body)
                .with_header("content-type: application/json".parse::<tiny_http::Header>().unwrap())
        })
    }

    fn start<F>(handler: F) -> Self
    where F: Fn(&mut tiny_http::Request) -> tiny_http::Response<std::io::Cursor<Vec<u8>>>
        + Send + 'static,
    {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind test server");
        let port = server.server_addr().to_ip().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let req_clone = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            for mut req in server.incoming_requests() {
                req_clone.fetch_add(1, Ordering::SeqCst);
                let resp = handler(&mut req);
                let _ = req.respond(resp);
            }
        });
        Self { port, requests, _thread: handle }
    }

    fn url(&self) -> String { format!("http://127.0.0.1:{}", self.port) }
    fn count(&self) -> usize { self.requests.load(Ordering::SeqCst) }
}

#[test]
fn ollama_embedder_round_trips_a_single_prompt() {
    let server = TestServer::start_ollama();
    let e = HttpEmbedder::new(server.url(), Provider::Ollama, "nomic-embed-text");
    let v = e.embed("hello").unwrap();
    assert_eq!(v.len(), 4);
    assert_eq!(v[0], 5.0); // len("hello")
    assert_eq!(v[3], 1.0);
    assert_eq!(server.count(), 1);
}

#[test]
fn ollama_embedder_loops_per_text_for_batches() {
    // Ollama's /api/embeddings is non-batchable; the embedder loops
    // and we observe one HTTP request per text.
    let server = TestServer::start_ollama();
    let e = HttpEmbedder::new(server.url(), Provider::Ollama, "any");
    let _ = e.embed_batch(&["a", "bb", "ccc"]).unwrap();
    assert_eq!(server.count(), 3);
}

#[test]
fn openai_embedder_sends_one_batch_request() {
    let server = TestServer::start_openai();
    let e = HttpEmbedder::new(server.url(), Provider::OpenAi, "text-embedding-3-small");
    let out = e.embed_batch(&["alpha", "beta", "gamma"]).unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[0][0], 5.0);  // len("alpha")
    assert_eq!(out[1][0], 4.0);  // len("beta")
    assert_eq!(out[2][0], 5.0);  // len("gamma")
    assert_eq!(server.count(), 1, "OpenAI-compat sends one request per batch");
}

#[test]
fn http_embedder_records_dim_from_first_response() {
    let server = TestServer::start_ollama();
    let e = HttpEmbedder::new(server.url(), Provider::Ollama, "any");
    // Default before first call.
    assert_ne!(e.dim(), 4);
    let _ = e.embed("anything").unwrap();
    assert_eq!(e.dim(), 4, "dim should reflect the first observed embedding length");
}

#[test]
fn caching_embedder_amortises_repeat_queries_against_http() {
    let server = TestServer::start_ollama();
    let inner = HttpEmbedder::new(server.url(), Provider::Ollama, "any");
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = lex_search::CachingEmbedder::new(
        inner, cache_dir.path().to_path_buf(), "ollama:any",
    );
    let _ = cache.embed("first").unwrap();
    let _ = cache.embed("second").unwrap();
    let after_two_unique = server.count();
    assert_eq!(after_two_unique, 2);

    // Now hit both again and a new one — only the new one should
    // reach the HTTP server.
    let _ = cache.embed_batch(&["first", "third", "second"]).unwrap();
    assert_eq!(server.count(), after_two_unique + 1);
}
