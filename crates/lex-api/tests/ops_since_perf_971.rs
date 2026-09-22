//! Timing harness for `/v1/ops/since` paging (#971). Ignored by default;
//! run with
//!
//! ```text
//! LEX_OPS_BENCH_N=50000 cargo test --release -p lex-api \
//!     --test ops_since_perf_971 -- --ignored --nocapture
//! ```
//!
//! Seeds a synthetic store of `N` ops (a main line with a feature branch
//! merged back every 50 ops) and times a full pull at the client's page
//! size, once with the legacy `after=<last op>` protocol and once
//! following `X-Lex-Next-Cursor` when the server sends it, each against a
//! fresh server (cold cache). Also reports how many distinct ops each
//! protocol delivered. Before #971 legacy paging over merges repeated and
//! dropped ops; now both must deliver every op exactly once.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use lex_api::handlers::State;
use lex_vcs::{OpLog, Operation, OperationKind, OperationRecord, StageTransition};
use tempfile::TempDir;

const PAGE: usize = 1000;

fn get(addr: &SocketAddr, path: &str) -> (Option<String>, Vec<String>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
    // HTTP/1.0 so the body arrives unchunked.
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    assert!(head.contains(" 200 "), "{head}");
    let next = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("x-lex-next-cursor"))
        .map(|(_, v)| v.trim().to_string());
    let recs: Vec<OperationRecord> = serde_json::from_str(body).unwrap();
    (next, recs.into_iter().map(|r| r.op_id).collect())
}

fn op(parents: Vec<String>, i: usize) -> OperationRecord {
    let kind = if parents.len() == 2 {
        OperationKind::Merge { resolved: i }
    } else {
        OperationKind::AddFunction {
            sig_id: format!("sig-{i}"),
            stage_id: format!("stg-{i}"),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        }
    };
    let produces = StageTransition::Create { sig_id: format!("sig-{i}"), stage_id: format!("stg-{i}") };
    OperationRecord::new(Operation::new(kind, parents), produces)
}

/// Write a record the way `OpLog::put` lays it out, minus the per-file
/// fsync (which would dominate seeding 50k ops).
fn put(root: &std::path::Path, rec: &OperationRecord) {
    std::fs::write(root.join(format!("ops/{}.json", rec.op_id)), serde_json::to_vec(rec).unwrap()).unwrap();
}

fn seed(root: &std::path::Path, n: usize) -> String {
    let _ = OpLog::open(root).unwrap();
    let mut main = op(vec![], 0);
    put(root, &main);
    let mut feature = main.op_id.clone();
    let mut i = 1;
    while i < n {
        let next = if i % 50 == 0 {
            // Merge the feature line back into main.
            let m = op(vec![main.op_id.clone(), feature.clone()], i);
            feature = m.op_id.clone();
            m
        } else if i % 5 == 0 {
            let f = op(vec![feature.clone()], i);
            feature = f.op_id.clone();
            put(root, &f);
            i += 1;
            continue;
        } else {
            op(vec![main.op_id.clone()], i)
        };
        put(root, &next);
        main = next;
        i += 1;
    }
    let path = root.join("branches/main.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let v = serde_json::json!({"name":"main","parent":null,"head_op":main.op_id,"merges":[],"created_at":0});
    std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    main.op_id
}

/// `(ops received, distinct ops received, per-page latency)`.
fn pull(addr: &SocketAddr, follow_cursor: bool) -> (usize, usize, Vec<Duration>) {
    let mut times = Vec::new();
    let mut total = 0;
    let mut distinct = std::collections::HashSet::new();
    let mut after: Option<String> = None;
    let mut cursor: Option<String> = None;
    loop {
        let mut path = format!("/v1/ops/since?branch=main&limit={PAGE}");
        if let Some(a) = &after {
            path.push_str(&format!("&after={a}"));
        }
        if let Some(c) = &cursor {
            path.push_str(&format!("&cursor={c}"));
        }
        let t = Instant::now();
        let (next, page) = get(addr, &path);
        times.push(t.elapsed());
        total += page.len();
        distinct.extend(page.iter().cloned());
        if page.len() < PAGE {
            break;
        }
        after = page.last().cloned();
        // A server that has sent a cursor ends the pull by omitting one;
        // one that never sends any (pre-#971) is paged by `after` alone.
        if follow_cursor && cursor.is_some() && next.is_none() {
            break;
        }
        cursor = if follow_cursor { next } else { None };
    }
    (total, distinct.len(), times)
}

fn report(label: &str, (total, distinct, times): (usize, usize, Vec<Duration>)) -> usize {
    let sum: Duration = times.iter().sum();
    let ms = |d: &Duration| d.as_secs_f64() * 1e3;
    let mid = &times[times.len() / 2];
    let times = &times;
    println!(
        "{label}: {total} ops ({distinct} distinct) in {} pages, total {:.2}s; first page {:.0}ms, middle page {:.0}ms, last page {:.0}ms",
        times.len(),
        sum.as_secs_f64(),
        ms(&times[0]),
        ms(mid),
        ms(times.last().unwrap()),
    );
    distinct
}

fn serve(root: &std::path::Path) -> SocketAddr {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!(),
    };
    let state = Arc::new(State::open(root.to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    addr
}

#[test]
#[ignore = "timing harness; run explicitly with --ignored --nocapture"]
fn time_a_full_pull() {
    let n: usize = std::env::var("LEX_OPS_BENCH_N").ok().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let tmp = TempDir::new().unwrap();
    let t = Instant::now();

    let head = seed(tmp.path(), n);
    println!("seeded {n} ops in {:.1}s", t.elapsed().as_secs_f64());
    let reachable = OpLog::open(tmp.path()).unwrap().ops_since(&head, None).unwrap().len();
    println!("{reachable} ops reachable from main");

    // A fresh server (cold cache) per protocol.
    let (legacy_total, legacy_distinct, times) = pull(&serve(tmp.path()), false);
    report("legacy after= paging", (legacy_total, legacy_distinct, times));
    let (cursor_total, cursor_distinct, times) = pull(&serve(tmp.path()), true);
    report("cursor paging       ", (cursor_total, cursor_distinct, times));
    // Both protocols: every reachable op, exactly once (#971).
    assert_eq!((legacy_total, legacy_distinct), (reachable, reachable));
    assert_eq!((cursor_total, cursor_distinct), (reachable, reachable));
}
