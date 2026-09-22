//! #971: `lex op pull` against a hub that is cold-starting. A proxy's
//! empty-bodied 502 must be retried on an idempotent fetch, and when it
//! persists the error must name the status and endpoint — never the old
//! `json: EOF while parsing a value at line 1 column 0`.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

/// A stub remote: the n-th request gets `script[min(n, last)]`.
fn stub(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind stub");
    let port = server.server_addr().to_ip().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let n = h.fetch_add(1, Ordering::SeqCst);
            let (status, body) = script[n.min(script.len() - 1)];
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(status));
        }
    });
    (format!("http://127.0.0.1:{port}"), hits)
}

fn pull(remote: &str, retries: &str) -> std::process::Output {
    let store = tempdir().unwrap();
    Command::new(lex_bin())
        .args(["--output", "json", "op", "pull", remote, "--dry-run"])
        .args(["--store", store.path().to_str().unwrap()])
        .env("LEX_SYNC_RETRIES", retries)
        .env_remove("LEXHUB_TOKEN")
        .output()
        .unwrap()
}

#[test]
fn pull_rides_out_a_cold_start_502() {
    let (remote, hits) = stub(vec![(502, ""), (200, "[]")]);
    let out = pull(&remote, "3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one retry after the 502");
    assert!(
        stderr.contains("/v1/ops/since: HTTP 502 — retrying"),
        "stderr: {stderr}"
    );
}

#[test]
fn pull_recovers_from_an_empty_200_body() {
    let (remote, hits) = stub(vec![(200, ""), (200, "[]")]);
    let out = pull(&remote, "3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(
        stderr.contains("empty response body — retrying"),
        "stderr: {stderr}"
    );
}

#[test]
fn persistent_502_names_status_and_endpoint() {
    let (remote, hits) = stub(vec![(502, "")]);
    let out = pull(&remote, "0");
    // `--output json` reports the error as an envelope on stdout.
    let stderr = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "LEX_SYNC_RETRIES=0 disables retry"
    );
    assert!(
        stderr.contains(&format!("GET {remote}/v1/ops/since")),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("HTTP 502"), "stderr: {stderr}");
    assert!(!stderr.contains("EOF while parsing"), "stderr: {stderr}");
}
