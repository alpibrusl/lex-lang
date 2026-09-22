//! `lex store search` over a *pulled* store (#969).
//!
//! Lifecycle status (Draft/Active/Deprecated/Tombstone) is local to a store:
//! it is not an op and is not carried by `op push`/`op pull`. A pushed stage
//! lands Draft on the server, and a pulled one lands Draft in the consumer —
//! even when the author ran `publish --activate`. `lex store search` ranks
//! only Active stages, so on a pulled store it used to return **0 hits with no
//! diagnostic**, reading as "no matches" instead of "search can't see your
//! data".
//!
//! This drives the real binary against a real `lex-api` server:
//!
//!   author store --op push--> server --op pull--> consumer store
//!
//! and pins that search on the consumer (a) says why it found nothing and how
//! to see the drafts, and (b) with `--include-draft` finds the pulled function,
//! surfacing its status, and picks the version the branch head names.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

struct Server {
    addr: SocketAddr,
    tmp: TempDir,
}

fn start_server() -> Server {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(50));
    Server { addr, tmp }
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        eprintln!(
            "`lex {}` failed:\n  stdout: {stdout}\n  stderr: {stderr}",
            args.join(" ")
        );
    }
    (out.status.success(), stdout, stderr)
}

fn publish(store: &Path, src_path: &Path, src: &str) {
    std::fs::write(src_path, src).unwrap();
    let (ok, _, _) = run(&[
        "publish",
        "--store",
        store.to_str().unwrap(),
        "--branch",
        "main",
        "--activate",
        src_path.to_str().unwrap(),
    ]);
    assert!(ok, "publish must succeed");
}

fn search_json(store: &Path, extra: &[&str], query: &str) -> serde_json::Value {
    let mut args = vec![
        "--output",
        "json",
        "store",
        "search",
        "--store",
        store.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    args.push(query);
    let (ok, stdout, _) = run(&args);
    assert!(ok, "search must succeed");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    v.get("data").cloned().unwrap_or(v)
}

/// Author publishes (and activates) two functions, pushes; a fresh consumer
/// pulls. Returns (tmp, server, consumer store path).
fn pulled_consumer() -> (TempDir, Server, PathBuf) {
    let srv = start_server();
    let url = format!("http://{}", srv.addr);
    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");
    publish(
        &author,
        &src,
        "fn parse_csv(text :: String) -> List[String] { [] }\n\
         fn send_post(url :: String, body :: String) -> String { url }\n",
    );
    let (ok, _, _) = run(&["op", "push", &url, "--store", author.to_str().unwrap()]);
    assert!(ok, "push must succeed");
    let (ok, _, _) = run(&["op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(ok, "pull must succeed");
    (tmp, srv, consumer)
}

#[test]
fn pulled_stages_keep_the_source_lifecycle_which_is_draft() {
    // Documents the finding behind #969's fix direction: lifecycle is not
    // transmitted, so even an author-side `--activate` arrives Draft on the
    // server *and* in the puller. The pulled store faithfully mirrors its
    // source (the server) — neither has an Active stage.
    let (_tmp, srv, consumer) = pulled_consumer();
    for root in [srv.tmp.path(), consumer.as_path()] {
        let store = lex_store::Store::open(root).unwrap();
        let head = store.branch_head("main").unwrap();
        assert_eq!(
            head.len(),
            2,
            "both functions reach the head at {}",
            root.display()
        );
        for (sig, stage) in &head {
            assert_eq!(
                store.resolve_sig(sig).unwrap(),
                None,
                "no Active stage for {sig}"
            );
            assert_eq!(
                store.get_status(stage).unwrap(),
                lex_store::StageStatus::Draft
            );
        }
    }
}

#[test]
fn search_on_pulled_store_explains_zero_hits() {
    let (_tmp, _srv, consumer) = pulled_consumer();

    let v = search_json(&consumer, &[], "parse csv");
    assert_eq!(
        v["hits"].as_array().unwrap().len(),
        0,
        "default search ranks Active only"
    );
    assert_eq!(
        v["drafts_skipped"], 2,
        "both pulled drafts are reported as skipped: {v}"
    );
    let hint = v["hint"]
        .as_str()
        .expect("a zero-hit search over drafts must carry a hint");
    assert!(
        hint.contains("--include-draft"),
        "hint names the flag: {hint}"
    );

    // Same diagnostic in text mode — that is where "0 hits" read as "no matches".
    let (ok, stdout, _) = run(&[
        "store",
        "search",
        "--store",
        consumer.to_str().unwrap(),
        "parse csv",
    ]);
    assert!(ok);
    assert!(
        stdout.contains("--include-draft"),
        "text output must carry the hint: {stdout}"
    );
    assert!(
        stdout.contains("2 draft"),
        "text output must count the drafts: {stdout}"
    );
}

#[test]
fn include_draft_finds_pulled_functions_and_surfaces_status() {
    let (_tmp, _srv, consumer) = pulled_consumer();

    let v = search_json(&consumer, &["--include-draft"], "parse csv");
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "both pulled functions are searchable: {v}");
    assert_eq!(
        hits[0]["name"], "parse_csv",
        "query ranks parse_csv on top: {v}"
    );
    assert_eq!(
        hits[0]["status"], "draft",
        "status is surfaced in results: {v}"
    );
    assert_eq!(v["drafts_skipped"], 0);
    assert!(
        v.get("hint").is_none() || v["hint"].is_null(),
        "no hint once drafts are included: {v}"
    );

    let (ok, stdout, _) = run(&[
        "store",
        "search",
        "--store",
        consumer.to_str().unwrap(),
        "--include-draft",
        "parse csv",
    ]);
    assert!(ok);
    assert!(
        stdout.contains("parse_csv") && stdout.contains("[draft]"),
        "text output lists the hit and marks it draft: {stdout}"
    );
}

#[test]
fn include_draft_picks_the_version_the_branch_head_names() {
    // Two versions of one function share a SigId; both are pulled as Draft.
    // `--include-draft` must index the one the head names, not an arbitrary
    // (pull order is by stage id, not by history) older draft.
    let srv = start_server();
    let url = format!("http://{}", srv.addr);
    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");
    publish(&author, &src, "fn scale(x :: Int) -> Int { x * 2 }\n");
    publish(&author, &src, "fn scale(x :: Int) -> Int { x * 3 }\n");
    publish(&author, &src, "fn scale(x :: Int) -> Int { x * 4 }\n");
    assert!(run(&["op", "push", &url, "--store", author.to_str().unwrap()]).0);
    assert!(run(&["op", "pull", &url, "--store", consumer.to_str().unwrap()]).0);

    let store = lex_store::Store::open(&consumer).unwrap();
    let head = store.branch_head("main").unwrap();
    assert_eq!(head.len(), 1);
    let (sig, head_stage) = head.iter().next().unwrap();
    let drafts = store.sig_history(sig).unwrap();
    assert_eq!(drafts.len(), 3, "all three versions were pulled");

    let v = search_json(&consumer, &["--include-draft"], "scale");
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "one function, one hit: {v}");
    assert_eq!(
        hits[0]["stage_id"].as_str(),
        Some(head_stage.as_str()),
        "the head's version is the one indexed: {v}"
    );
}

#[test]
fn local_active_store_has_no_hint_negative_control() {
    // A normally-authored store: everything Active, so nothing is skipped and
    // no hint is emitted. Guards against a hint that fires unconditionally.
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().join("s");
    publish(
        &store,
        &tmp.path().join("lib.lex"),
        "fn parse_csv(text :: String) -> List[String] { [] }\n",
    );
    let v = search_json(&store, &[], "parse csv");
    assert_eq!(v["hits"].as_array().unwrap().len(), 1);
    assert_eq!(v["hits"][0]["status"], "active");
    assert_eq!(v["drafts_skipped"], 0);
    assert!(
        v.get("hint").is_none() || v["hint"].is_null(),
        "no hint on an all-Active store: {v}"
    );
}
