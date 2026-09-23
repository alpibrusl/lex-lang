//! #1007 PR 5: `lex op push`/`lex op pull` sync a `SetFiles` op's blob
//! contents, not just its op record.
//!
//! Before this PR, pushing a directory publish that captured files (#1007
//! PR 4's default) 422'd with `MissingBlobs` — the server-side validation
//! from PR 3 correctly rejecting an incomplete `SetFiles` push, because the
//! client never uploaded the manifest/entry blobs the op names. This file
//! exercises the fix end to end against the real `lex-api` handler code
//! (the same in-process test-hub pattern `op_push_lock_sync_1031.rs` and
//! `pkg_transitive_archive_1031.rs` use):
//!
//! * a full push → pull into a fresh store reproduces every captured file's
//!   bytes and mode exactly;
//! * a second push, after changing exactly one file, uploads only that
//!   file's new blob (+ the new manifest blob) — not the whole file set;
//! * a push of a `SetFiles` op to a hub that doesn't advertise `files-v1`
//!   refuses BEFORE uploading anything;
//! * a pull re-hashes every blob it receives and rejects the whole pull if
//!   the hub serves corrupted/tampered bytes under a blob's claimed id.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── the real in-process lex-api hub (files-v1 capable) ──────────────────────

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || lex_api::serve_on(server, state));
    wait_until_serving(&addr);
    (Server { addr, _join: Some(join) }, tmp)
}

fn wait_until_serving(addr: &SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
}

// ── running the real `lex` binary ────────────────────────────────────────

fn run_lex(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    Command::new(lex_bin())
        .current_dir(cwd)
        .env("HOME", env_root)
        .env("LEX_PACKAGES_DIR", env_root.join("packages"))
        .env_remove("LEX_STORE")
        .env_remove("LEXHUB_TOKEN")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
}

fn ok(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    let out = run_lex(cwd, env_root, args);
    assert!(
        out.status.success(),
        "`lex {}` failed (cwd={}):\nstdout: {}\nstderr: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn json_ok(cwd: &Path, env_root: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = vec!["--output", "json"];
    full.extend_from_slice(args);
    let out = ok(cwd, env_root, &full);
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("non-JSON from lex {args:?}: {e}\nstdout: {text}"))
}

/// Unwrap the `data` envelope `acli::emit_or_text` wraps successes in.
fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

// ── test fixture: a small package with a realistic non-op-log file set ─────

/// Writes `pkg.lex.toml` + `src/lib.lex` + a handful of non-op-log files
/// covering the interesting cases §2 calls out: a text file, a nested
/// `tests/` file, an executable script (mode 100755), and binary content
/// (invalid UTF-8) — proving files-v1 round-trips exact bytes, not just
/// text. Returns the map of relative path -> original bytes, so callers can
/// assert byte-for-byte fidelity after a pull.
fn write_fixture_pkg(dir: &Path, name: &str) -> Vec<(String, Vec<u8>)> {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    let toml = format!("[package]\nname = \"{name}\"\nversion = \"1.0.0\"\n");
    std::fs::write(dir.join("lex.toml"), toml.as_bytes()).unwrap();
    files.push(("lex.toml".to_string(), toml.into_bytes()));

    std::fs::write(dir.join("src/lib.lex"), b"fn f(x :: Int) -> Int { x }\n").unwrap();
    // src/lib.lex is op-log-owned (reserved path) -- deliberately NOT in
    // `files`, which only tracks what the files manifest should hold.

    let readme = b"# filesroundtrip\n\nA #1007 PR 5 fixture package.\n".to_vec();
    std::fs::write(dir.join("README.md"), &readme).unwrap();
    files.push(("README.md".to_string(), readme));

    let test_txt = b"basic test fixture content\n".to_vec();
    std::fs::write(dir.join("tests/basic.txt"), &test_txt).unwrap();
    files.push(("tests/basic.txt".to_string(), test_txt));

    let script = b"#!/bin/sh\necho hello\n".to_vec();
    let script_path = dir.join("bin/run.sh");
    std::fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    files.push(("bin/run.sh".to_string(), script));

    // Binary content: invalid UTF-8 bytes, proving files-v1 handles exact
    // bytes rather than text (#1007 §2: "Binary allowed, exact bytes").
    let binary: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x9F, 0x00, 0x01, 0x02, 0xC0, 0x80, 0xFF];
    std::fs::write(dir.join("assets/logo.bin"), &binary).unwrap();
    files.push(("assets/logo.bin".to_string(), binary));

    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn files_ls(cwd: &Path, env_root: &Path, store: &str) -> Vec<serde_json::Value> {
    let v = json_ok(cwd, env_root, &["files", "ls", "--store", store]);
    data(&v)["entries"].as_array().cloned().unwrap_or_default()
}

fn files_cat_b64(cwd: &Path, env_root: &Path, store: &str, path: &str) -> Vec<u8> {
    use base64::Engine as _;
    let v = json_ok(cwd, env_root, &["files", "cat", "--store", store, path]);
    let b64 = data(&v)["content_b64"].as_str().unwrap_or_else(|| panic!("no content_b64 for {path}: {v}"));
    base64::engine::general_purpose::STANDARD.decode(b64).unwrap()
}

// ── 1. full round trip: push, then pull into a fresh store ─────────────────

#[test]
fn push_then_pull_into_a_fresh_store_reproduces_every_file_exactly() {
    let (server, _hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let pkg = work.path().join("pkg");
    let expected_files = write_fixture_pkg(&pkg, "filesroundtrip1007");
    let store_a = pkg.join(".lex/store").to_string_lossy().into_owned();

    let published = json_ok(&pkg, env_root.path(), &["publish", "--store", &store_a, "--activate", "."]);
    assert!(
        !data(&published)["files_manifest"].is_null(),
        "a directory publish with non-op-log files must capture a files manifest by default \
         (no --no-files here): {published}"
    );

    let pushed = json_ok(&pkg, env_root.path(), &["op", "push", &hub, "--store", &store_a]);
    let blobs_pushed = data(&pushed)["blobs_pushed"].as_u64().unwrap_or(0);
    assert!(
        blobs_pushed >= expected_files.len() as u64,
        "expected at least one blob per captured file (manifest + {} entries), got {blobs_pushed}: {pushed}",
        expected_files.len(),
    );

    // Pull into a store that has never seen this package before.
    let fresh_dir = work.path().join("fresh");
    let store_b = fresh_dir.join(".lex/store").to_string_lossy().into_owned();
    std::fs::create_dir_all(&fresh_dir).unwrap();
    let pulled = json_ok(&fresh_dir, env_root.path(), &["op", "pull", &hub, "--store", &store_b]);
    assert!(
        data(&pulled)["blobs_pulled"].as_u64().unwrap_or(0) > 0,
        "the pull must have fetched blobs for the SetFiles op it received: {pulled}"
    );

    // `lex files ls` must agree on every path/blob/mode/size.
    let ls_a = files_ls(&pkg, env_root.path(), &store_a);
    let ls_b = files_ls(&fresh_dir, env_root.path(), &store_b);
    assert_eq!(
        ls_a, ls_b,
        "the pulled store's files manifest must be identical to the pushed one"
    );
    assert_eq!(ls_a.len(), expected_files.len(), "unexpected entry count: {ls_a:?}");

    // And the actual bytes: original on-disk content == store A's blob ==
    // store B's blob, for every captured file (including the binary one).
    for (path, original) in &expected_files {
        let from_a = files_cat_b64(&pkg, env_root.path(), &store_a, path);
        let from_b = files_cat_b64(&fresh_dir, env_root.path(), &store_b, path);
        assert_eq!(&from_a, original, "store A's `{path}` must match the original bytes");
        assert_eq!(&from_b, original, "the PULLED store's `{path}` must match the original bytes exactly");
    }

    // The executable bit is part of the manifest (`mode`), not just content.
    let exec_entry = ls_b
        .iter()
        .find(|e| e["path"].as_str() == Some("bin/run.sh"))
        .unwrap_or_else(|| panic!("bin/run.sh missing from pulled manifest: {ls_b:?}"));
    assert_eq!(exec_entry["mode"].as_str(), Some("100755"), "exec bit must round-trip: {exec_entry}");
}

// ── 2. incremental push uploads only the changed blob ───────────────────────

#[test]
fn incremental_push_uploads_only_the_changed_blob() {
    let (server, hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let pkg = work.path().join("pkg");
    let expected_files = write_fixture_pkg(&pkg, "filesincremental1007");
    let store = pkg.join(".lex/store").to_string_lossy().into_owned();

    ok(&pkg, env_root.path(), &["publish", "--store", &store, "--activate", "."]);
    let first_push = json_ok(&pkg, env_root.path(), &["op", "push", &hub, "--store", &store]);
    let first_blobs_pushed = data(&first_push)["blobs_pushed"].as_u64().unwrap_or(0);
    assert!(first_blobs_pushed >= expected_files.len() as u64, "{first_push}");

    let blobs_dir = hub_tmp.path().join("blobs");
    let count_blobs = |dir: &Path| -> usize {
        std::fs::read_dir(dir).map(|rd| rd.count()).unwrap_or(0)
    };
    let hub_blob_count_after_first = count_blobs(&blobs_dir);

    // Change exactly one file (README.md) and republish + push again.
    std::fs::write(pkg.join("README.md"), b"# filesincremental\n\nchanged content.\n").unwrap();
    ok(&pkg, env_root.path(), &["publish", "--store", &store, "--activate", "."]);
    let second_push = json_ok(&pkg, env_root.path(), &["op", "push", &hub, "--store", &store]);
    let second_blobs_pushed = data(&second_push)["blobs_pushed"].as_u64().unwrap_or(u64::MAX);

    // Exactly the changed file's new blob + the new manifest blob -- never
    // the other, UNCHANGED files' blobs (which the remote already has).
    assert_eq!(
        second_blobs_pushed, 2,
        "an incremental push (one file changed) must upload only its new blob \
         + the new manifest blob, not the whole file set: {second_push}"
    );

    // Independent, server-side confirmation: the hub's own blob store grew
    // by exactly 2 files (the new README blob + the new manifest blob), not
    // by the full file count again.
    let hub_blob_count_after_second = count_blobs(&blobs_dir);
    assert_eq!(
        hub_blob_count_after_second - hub_blob_count_after_first,
        2,
        "the hub's on-disk blob store must have gained exactly 2 new blobs"
    );
}

// ── 3. a caps-less server refuses BEFORE uploading anything ─────────────────

/// A minimal hand-rolled HTTP stub — NOT the real lex-api hub — that
/// announces zero capabilities on `/v1/health` and otherwise ONLY answers
/// the branch-head probe `op push` needs to compute its delta. Any other
/// request (a stage/blob/ops upload) increments `unexpected_hits`, which
/// the test asserts stays at zero: the whole point is that `op push` must
/// never get that far.
fn start_caps_less_stub() -> (SocketAddr, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let unexpected_hits = Arc::new(AtomicUsize::new(0));
    let hits = Arc::clone(&unexpected_hits);
    let join = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = [0u8; 8192];
            let n = match stream.read(&mut buf) {
                Ok(0) => continue, // a bare connect-then-close readiness probe, not a request
                Ok(n) => n,
                Err(_) => continue,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let first_line = req.lines().next().unwrap_or("");
            let mut parts = first_line.split_whitespace();
            let _method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("");
            let path = path.split('?').next().unwrap_or(path);

            let (status, body) = if path == "/v1/health" {
                (200, r#"{"ok":true,"caps":[]}"#.to_string())
            } else if path.starts_with("/v1/branches/") && path.ends_with("/head") {
                (200, r#"{"head_op":null}"#.to_string())
            } else {
                hits.fetch_add(1, Ordering::SeqCst);
                (404, r#"{"error":"unexpected request in caps-less stub"}"#.to_string())
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    // Give the listener a moment to be ready to accept.
    for _ in 0..200 {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_ok() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    (addr, unexpected_hits, join)
}

#[test]
fn push_to_a_caps_less_server_refuses_before_uploading_anything() {
    let (addr, unexpected_hits, _join) = start_caps_less_stub();
    let remote = format!("http://{addr}");
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let pkg = work.path().join("pkg");
    write_fixture_pkg(&pkg, "filescapsless1007");
    let store = pkg.join(".lex/store").to_string_lossy().into_owned();

    ok(&pkg, env_root.path(), &["publish", "--store", &store, "--activate", "."]);

    let out = run_lex(&pkg, env_root.path(), &["op", "push", &remote, "--store", &store]);
    assert!(
        !out.status.success(),
        "pushing a SetFiles op to a caps-less remote must fail, not silently succeed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("files-v1") && stderr.contains("does not advertise"),
        "the refusal must name the missing capability: {stderr}"
    );
    assert_eq!(
        unexpected_hits.load(Ordering::SeqCst),
        0,
        "no upload request (stages/blobs/ops) must ever reach the remote — the refusal must \
         happen BEFORE any upload, purely from the /v1/health capability check"
    );
}

// ── 4. pull re-hashes and rejects a tampered blob ───────────────────────────

#[test]
fn pull_rejects_a_tampered_blob_and_leaves_the_branch_unadvanced() {
    let (server, hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let pkg = work.path().join("pkg");
    let expected_files = write_fixture_pkg(&pkg, "filestamper1007");
    let store = pkg.join(".lex/store").to_string_lossy().into_owned();

    ok(&pkg, env_root.path(), &["publish", "--store", &store, "--activate", "."]);
    ok(&pkg, env_root.path(), &["op", "push", &hub, "--store", &store]);

    // Corrupt the README blob directly on the hub's disk: flip one byte, so
    // the file is no longer the content its filename (the blob id) claims.
    let readme_bytes = expected_files
        .iter()
        .find(|(p, _)| p == "README.md")
        .map(|(_, b)| b.clone())
        .expect("fixture always writes README.md");
    let readme_id = sha256_hex(&readme_bytes);
    let blob_path = hub_tmp.path().join("blobs").join(&readme_id);
    assert!(blob_path.exists(), "the README blob must have been pushed to {}", blob_path.display());
    let mut corrupted = std::fs::read(&blob_path).unwrap();
    corrupted[0] ^= 0xFF;
    std::fs::write(&blob_path, &corrupted).unwrap();

    // Pull into a fresh store: must fail loudly, not silently accept the
    // tampered bytes.
    let fresh_dir = work.path().join("fresh");
    std::fs::create_dir_all(&fresh_dir).unwrap();
    let store_c = fresh_dir.join(".lex/store").to_string_lossy().into_owned();
    let out = run_lex(&fresh_dir, env_root.path(), &["op", "pull", &hub, "--store", &store_c]);
    assert!(
        !out.status.success(),
        "pulling a manifest whose blob was tampered with must fail, not silently succeed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("integrity") || stderr.contains("hash"),
        "the failure must be attributed to a blob integrity/hash mismatch: {stderr}"
    );

    // The branch must not have advanced in the fresh store: no head to walk.
    let log_out = run_lex(&fresh_dir, env_root.path(), &["--output", "json", "op", "log", "--store", &store_c]);
    let log_json: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&log_out.stdout).trim()).unwrap_or(serde_json::json!({}));
    let log_arr = data(&log_json)["log"].as_array().cloned().unwrap_or_default();
    assert!(
        log_arr.is_empty(),
        "a pull that fails the blob integrity check must leave the local branch head \
         unadvanced (no ops visible on `main`): {log_json}"
    );
}
