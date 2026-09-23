//! #1031 (adjacent finding 1), corrected by #1007 §0: `lex op push` still
//! syncs a *local* committed-lock change to the remote even when there are
//! zero new ops to push (`cmd_op_push` posts `/v1/locks/batch` outside the
//! `to_send.is_empty()` early return) — but a local publish may no longer
//! *produce* such a change out of thin air.
//!
//! #1031 originally relied on a flaw #1007 §0 identifies and fixes: a
//! semantically no-op `lex publish` used to call `set_committed_lock` on the
//! existing head regardless, silently attaching whatever `lex.lock` happened
//! to be on disk *right now* to a head that publish call did not itself
//! produce — including an already-pushed one. That is exactly the
//! "re-lock with zero new ops" trick this test used to exercise: change
//! `lex.lock` on disk, republish with no source change, and the old head's
//! lock silently followed along.
//!
//! §0's fix refuses that: `cmd_publish` only writes a head's committed lock
//! when *this* call actually produced (or is producing) that head — a
//! semantic no-op with nothing else new no longer touches it. So the second
//! half of this test now asserts the opposite of #1031's original claim:
//! a re-lock with zero new ops leaves the already-pushed head's lock
//! untouched, locally and therefore on the remote too. Updating a committed
//! lock at a *stable* head is no longer implicit; going forward it takes a
//! real op ([`lex files commit`], once files capture is on — see
//! `--no-files` below for why this test keeps it off).
//!
//! Both publishes here pass `--no-files`, DELIBERATELY, still — this is not
//! a #1007 PR 5 gap. #1007 PR 4 made a directory publish capture `lex.lock`
//! itself into a files manifest by default, and PR 5 (this op-log's own
//! `push_blobs`/`pull_blobs`) now syncs a `SetFiles` op's blob contents just
//! fine. But *this* test's second publish changes nothing but `lex.lock` on
//! disk, and with files capture on that IS a real (files-only) change: it
//! would legitimately emit its own `SetFiles` op, which makes `to_send`
//! non-empty and directly contradicts the "zero new ops" premise this test
//! is built to exercise (confirmed by trying it: the second `op push` then
//! reports `1 ops ... added`, not `nothing to push`). `--no-files` keeps
//! this test isolated to the op-DAG-level lock-sync mechanism it actually
//! tests; the files-capture push/pull path itself is covered by
//! `op_push_pull_files_1007.rs`.
//!
//! [`lex files commit`]: ../src/files.rs

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
fn a_relock_with_zero_new_ops_leaves_the_pushed_head_lock_untouched() {
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

    // --no-files here and below: see the module doc comment — this test's
    // "zero new ops" premise only holds with files capture off.
    ok(&pkg, env_root.path(), &["publish", "--no-files", "."]);
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
    ok(&pkg, env_root.path(), &["publish", "--no-files", "."]);
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

    // #1007 §0: the second publish produced no ops at all (source unchanged,
    // --no-files), so it must not have rewritten `head_op`'s committed lock
    // — even though `lex.lock` on disk now says v2. `op push`'s lock-sync
    // step still runs (it always does, per #1031), but it has nothing new to
    // sync: the LOCAL committed lock at this head is still v1.
    let second = fetch(&server.addr).expect("the head must still have its original lock");
    assert!(
        second.contains("op_v1") && !second.contains("op_v2"),
        "a semantically no-op republish must NOT rewrite an already-pushed \
         head's committed lock, even though lex.lock changed on disk \
         (#1007 §0) — got: {second}"
    );
}
