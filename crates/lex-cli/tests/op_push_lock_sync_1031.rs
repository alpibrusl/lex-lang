//! #1031 (adjacent finding 1): `lex op push` must sync the local head's
//! committed `lex.lock` even when there are zero new ops to push.
//!
//! `cmd_op_push` (`crates/lex-cli/src/op.rs`) used to post `/v1/locks/batch`
//! only in the branch that also pushes new ops — the `to_send.is_empty()`
//! case returned early, before ever reaching the lock-sync step. A re-lock
//! (`lex pkg lock`) of an unchanged, already-pushed head moves nothing in the
//! op DAG (no new ops), but the *committed lock* at that head can still
//! change underneath it (a dependency re-resolved to a new pin) — and that
//! change never reached the remote. This drives the real CLI end to end
//! against a real in-process `lex-api` server and checks the server's
//! stored lock via `POST /v1/locks/fetch`, the same surface `op push` posts
//! to.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

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

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, resp_body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, resp_body.to_string())
}

fn run_lex(cwd: &Path, env_root: &Path, args: &[&str]) -> std::process::Output {
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

fn ok(cwd: &Path, env_root: &Path, args: &[&str]) -> std::process::Output {
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

#[test]
fn a_relock_with_zero_new_ops_still_syncs_the_lock() {
    let (server, _hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let pkg = work.path().join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("lex.toml"),
        "[package]\nname = \"relockcli\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/lib.lex"), "fn f(x :: Int) -> Int { x }\n").unwrap();

    // An arbitrary, hand-written lock (its content doesn't need to satisfy
    // any declared dependency — `lex publish` just reads whatever's on disk
    // and commits it at the new head; only the sync mechanics are under
    // test here, not lock resolution itself).
    let lock_path = pkg.join("lex.lock");
    std::fs::write(
        &lock_path,
        "version = 1\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\nhead_op = \"op_v1\"\n",
    )
    .unwrap();

    ok(&pkg, env_root.path(), &["publish", "."]);
    ok(&pkg, env_root.path(), &["op", "push", &hub]);

    // The head this package published to, so we can ask the server for its
    // committed lock directly (the same surface `op push` posts the lock
    // to: `POST /v1/locks/fetch`).
    let (status, body) = http(&server.addr, "GET", "/v1/branches/main/head", "");
    assert_eq!(status, 200, "branch head probe: {body}");
    let head: serde_json::Value = serde_json::from_str(&body).unwrap();
    let head_op = head["head_op"].as_str().expect("head_op").to_string();

    let fetch = |addr: &SocketAddr| -> Option<String> {
        let (status, body) = http(
            addr,
            "POST",
            "/v1/locks/fetch",
            &serde_json::json!({ "head_ops": [head_op] }).to_string(),
        );
        assert_eq!(status, 200, "locks/fetch: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        v["locks"][&head_op].as_str().map(|s| s.to_string())
    };

    let first = fetch(&server.addr).expect("lock v1 must have synced on the first push");
    assert!(first.contains("op_v1"), "expected v1 lock, got: {first}");

    // Re-lock: same source (no new ops), but a different committed lock at
    // the SAME head — exactly `lex pkg lock` re-resolving a dependency to a
    // new pin, then re-publishing without touching the source.
    std::fs::write(
        &lock_path,
        "version = 1\n\n[[package]]\nname = \"dep\"\nversion = \"0.2.0\"\nhead_op = \"op_v2\"\n",
    )
    .unwrap();
    ok(&pkg, env_root.path(), &["publish", "."]);
    let push_out = ok(&pkg, env_root.path(), &["op", "push", &hub]);
    let push_text = format!(
        "{}{}",
        String::from_utf8_lossy(&push_out.stdout),
        String::from_utf8_lossy(&push_out.stderr)
    );
    assert!(
        push_text.contains("nothing to push"),
        "this push must see zero new ops (source is unchanged): {push_text}"
    );

    let second = fetch(&server.addr).expect("lock v2 must have synced on the zero-ops push");
    assert!(
        second.contains("op_v2"),
        "a re-lock with zero new ops must still sync the new committed lock \
         to the remote (#1031), got: {second}"
    );
}
