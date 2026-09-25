//! #892 PR 2: an intent's `origin` (git-import provenance) survives
//! `lex op push` / `lex op pull` against a real lex-api hub, and `lex op
//! push` refuses to send an origin-bearing intent to a hub that does not
//! advertise `intent-origin-v1` — BEFORE uploading anything. Pushes with no
//! origin-bearing intent are unaffected by a cap-less hub.
//!
//! The origin-bearing store is built in-process (the importer that will
//! write real ones is a later PR): one function published through
//! `Store::publish_program_with_intent` under an intent that carries an
//! `Origin`.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{Intent, IntentLog, ModelDescriptor, Origin, Person};
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── the real in-process lex-api hub ─────────────────────────────────────────

struct Hub {
    addr: SocketAddr,
    root: TempDir,
}

fn start_hub() -> Hub {
    let root = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(root.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    wait_until_serving(&addr);
    Hub { addr, root }
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

// ── a cap-less hub: forwards to a real hub, but /v1/health says caps: [] ────

/// Forwards every request to `upstream` (a real lex-api hub) untouched,
/// EXCEPT `GET /v1/health`, which it answers itself with `caps: []` — an
/// old hub that predates `intent-origin-v1`. Records `"METHOD path"` for
/// every request so tests can assert what did (not) reach the hub.
fn start_capless_proxy(upstream: SocketAddr) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut client) = stream else { break };
            client.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let Some((head, body)) = read_request(&mut client) else { continue };
            let first = head.lines().next().unwrap_or("").to_string();
            let mut parts = first.split_whitespace();
            let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let path = target.split('?').next().unwrap_or(target).to_string();
            log.lock().unwrap().push(format!("{method} {path}"));

            if method == "GET" && path == "/v1/health" {
                let b = r#"{"ok":true,"caps":[]}"#;
                let _ = client.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
                        b.len()
                    )
                    .as_bytes(),
                );
                continue;
            }

            // Rebuild the request for upstream: keep every header except
            // connection management, force `Connection: close` so the
            // response can be read to EOF.
            let mut req = format!("{method} {target} HTTP/1.1\r\n");
            for line in head.lines().skip(1) {
                let l = line.to_ascii_lowercase();
                if line.is_empty() || l.starts_with("connection:") || l.starts_with("proxy-") {
                    continue;
                }
                req.push_str(line);
                req.push_str("\r\n");
            }
            req.push_str("Connection: close\r\n\r\n");
            let mut up = TcpStream::connect(upstream).unwrap();
            up.set_read_timeout(Some(Duration::from_secs(10))).ok();
            up.write_all(req.as_bytes()).unwrap();
            up.write_all(&body).unwrap();
            let mut resp = Vec::new();
            let _ = up.read_to_end(&mut resp);
            let _ = client.write_all(&resp);
        }
    });
    (addr, seen)
}

/// Read one HTTP/1.1 request (headers + Content-Length body) off `s`.
fn read_request(s: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return None, // bare connect (readiness probe)
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    Some((head, body))
}

// ── running the real `lex` binary ───────────────────────────────────────────

fn run_lex(env_root: &Path, args: &[&str]) -> Output {
    Command::new(lex_bin())
        .env("HOME", env_root)
        .env("LEX_PACKAGES_DIR", env_root.join("packages"))
        .env_remove("LEX_STORE")
        .env_remove("LEXHUB_TOKEN")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
}

fn ok(env_root: &Path, args: &[&str]) -> Output {
    let out = run_lex(env_root, args);
    assert!(
        out.status.success(),
        "`lex {}` failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

// ── fixture: a store whose only intent carries (or lacks) an origin ────────

fn person(name: &str, when: i64, tz: &str) -> Person {
    Person { name: name.into(), email: "dev@example.com".into(), when, tz: tz.into() }
}

fn origin() -> Origin {
    Origin {
        vcs: "git".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        author: person("José Núñez", 1_700_000_000, "+0200"),
        committer: Some(person("Ann Committer", 1_700_000_100, "-0500")),
        parents: vec![
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ],
        folded: vec!["cccccccccccccccccccccccccccccccccccccccc".into()],
    }
}

fn import_intent(with_origin: bool) -> Intent {
    let i = Intent::with_timestamp(
        "add triple\n\nsecond paragraph",
        "git-import:1111111111111111111111111111111111111111",
        ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) },
        None,
        1_700_000_100,
    );
    if with_origin {
        i.with_origin(origin())
    } else {
        i
    }
}

/// Seed `<dir>` as a store with one published function under `intent`.
/// Returns the store root.
fn seed_store(dir: &Path, intent: &Intent) -> PathBuf {
    let root = dir.join("store");
    let store = lex_store::Store::open(&root).unwrap();
    IntentLog::open(&root).unwrap().put(intent).unwrap();
    let prog = lex_syntax::parse_source("fn triple(x :: Int) -> Int { x * 3 }\n").unwrap();
    let mut stages = lex_ast::canonicalize_program(&prog);
    lex_types::check_and_rewrite_program(&mut stages).expect("type-checks");
    let new_fns: std::collections::BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let report = lex_vcs::compute_diff(&Default::default(), &new_fns, false);
    let imports: lex_vcs::ImportMap = Default::default();
    store
        .publish_program_with_intent(
            lex_store::DEFAULT_BRANCH,
            &stages,
            &report,
            &imports,
            true,
            None,
            Some(intent.intent_id.clone()),
            &Default::default(),
        )
        .expect("publish");
    root
}

fn intent_files(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root.join("intents"))
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

// ── 1. real hub: origin round-trips through push and pull, id-stable ───────

#[test]
fn origin_round_trips_through_push_and_pull_against_a_real_hub() {
    let hub = start_hub();
    let url = format!("http://{}", hub.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let intent = import_intent(true);
    let src = seed_store(&work.path().join("src"), &intent);
    let src_s = src.to_string_lossy().into_owned();

    // The advertised capability is what the push gate looks for.
    ok(env_root.path(), &["op", "push", &url, "--store", &src_s]);

    // The hub holds the intent, origin intact, byte-identical to ours.
    let file = format!("{}.json", intent.intent_id);
    let local_bytes = std::fs::read(src.join("intents").join(&file)).unwrap();
    let hub_bytes = std::fs::read(hub.root.path().join("intents").join(&file))
        .expect("hub stored the intent under its id");
    assert_eq!(hub_bytes, local_bytes, "the hub must store exactly the bytes we sent");
    let on_hub: Intent = serde_json::from_slice(&hub_bytes).unwrap();
    assert_eq!(on_hub.origin, Some(origin()));

    // A fresh store pulls it back over /v1/intents/fetch.
    let dst_dir = work.path().join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();
    let dst = dst_dir.join("store");
    let dst_s = dst.to_string_lossy().into_owned();
    ok(env_root.path(), &["op", "pull", &url, "--store", &dst_s]);
    let pulled = IntentLog::open(&dst)
        .unwrap()
        .get(&intent.intent_id)
        .unwrap()
        .expect("the pulled store has the intent under the same id");
    assert_eq!(pulled, intent);
    assert_eq!(pulled.origin, Some(origin()));

    // Id-stable: rebuilding the intent from the pulled fields reproduces
    // the id it travelled under (nothing was rewritten in transit).
    let rebuilt = Intent::with_timestamp(
        pulled.prompt.clone(),
        pulled.session_id.clone(),
        pulled.model.clone(),
        pulled.parent_intent.clone(),
        pulled.created_at,
    )
    .with_origin(pulled.origin.clone().unwrap());
    assert_eq!(rebuilt.intent_id, intent.intent_id);
}

// ── 2. cap-less hub: origin-bearing push refused before any upload ─────────

#[test]
fn origin_bearing_push_to_a_capless_hub_is_refused_before_any_upload() {
    let hub = start_hub();
    let (proxy, seen) = start_capless_proxy(hub.addr);
    let remote = format!("http://{proxy}");
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let intent = import_intent(true);
    let src = seed_store(work.path(), &intent);
    let src_s = src.to_string_lossy().into_owned();

    let out = run_lex(env_root.path(), &["op", "push", &remote, "--store", &src_s]);
    assert!(!out.status.success(), "must refuse, not silently drop the provenance");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("intent-origin-v1") && stderr.contains("does not advertise"),
        "the refusal must name the missing capability: {stderr}"
    );

    // Only the delta probe and the capability check may have happened.
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|r| r == "GET /v1/health" || r.starts_with("GET /v1/branches/")),
        "no upload request may reach the hub before the refusal: {seen:?}"
    );
    assert!(seen.iter().any(|r| r == "GET /v1/health"), "the gate must consult /v1/health: {seen:?}");
    assert!(intent_files(hub.root.path()).is_empty(), "the hub must hold no intent");
}

// ── 3. cap-less hub: origin-free push is unaffected ─────────────────────────

#[test]
fn origin_free_push_to_a_capless_hub_still_succeeds() {
    let hub = start_hub();
    let (proxy, seen) = start_capless_proxy(hub.addr);
    let remote = format!("http://{proxy}");
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();

    let intent = import_intent(false);
    let src = seed_store(work.path(), &intent);
    let src_s = src.to_string_lossy().into_owned();

    ok(env_root.path(), &["op", "push", &remote, "--store", &src_s]);

    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().any(|r| r == "POST /v1/ops/batch"), "the push must have gone through: {seen:?}");
    assert!(
        !seen.iter().any(|r| r == "GET /v1/health"),
        "a push with no origin-bearing intent has no reason to query capabilities: {seen:?}"
    );
    assert!(intent_files(hub.root.path()).contains(&format!("{}.json", intent.intent_id)));
}
