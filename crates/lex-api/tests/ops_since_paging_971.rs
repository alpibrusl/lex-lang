//! `/v1/ops/since` paging contract (#971).
//!
//! A full pull pages through `/v1/ops/since`. These tests pin what a client
//! ends up with, for both paging protocols:
//!
//! * **Legacy `after=<last op of the previous page>`** (every released
//!   `lex op pull`) delivers every op of the delta **exactly once**, parents
//!   before children. That includes merge ops, a second branch's history,
//!   a cutoff on the other branch, and a cutoff the server never saw.
//!   **This is an intentional behaviour change.** The old rule re-derived
//!   each page from the last op's ancestry. On merge-heavy histories it
//!   repeated some ops and silently dropped others. The old rule is kept
//!   below as an oracle, and
//!   `legacy_paging_is_lossless_where_the_old_rule_was_not` shows it losing
//!   ops on a fixture the server now pages losslessly.
//! * **Resumable cursor** (`X-Lex-Next-Cursor` / `cursor=`) delivers the
//!   same sequence and pins the head the pull started from.
//! * **Unpaged** (no `limit`) is unchanged: exactly `OpLog::ops_since`
//!   reversed.
//!
//! Oracles are computed straight from the op log with `lex_vcs::OpLog`,
//! independent of the handler.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{OpLog, Operation, OperationKind, OperationRecord, StageTransition};
use tempfile::TempDir;

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
    thread::sleep(Duration::from_millis(20));
    Server { addr, tmp }
}

/// `(status, headers (lower-cased names), body)`.
fn get(addr: &SocketAddr, path: &str) -> (u16, BTreeMap<String, String>, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, body.to_string())
}

fn post(addr: &SocketAddr, path: &str, body: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn add(parents: &[&str], tag: &str) -> OperationRecord {
    OperationRecord::new(
        Operation::new(
            OperationKind::AddFunction {
                sig_id: format!("sig-{tag}"),
                stage_id: format!("stg-{tag}"),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            parents.iter().map(|p| p.to_string()),
        ),
        StageTransition::Create { sig_id: format!("sig-{tag}"), stage_id: format!("stg-{tag}") },
    )
}

fn merge(dst: &str, src: &str, resolved: usize) -> OperationRecord {
    OperationRecord::new(
        Operation::new(OperationKind::Merge { resolved }, [dst.to_string(), src.to_string()]),
        StageTransition::Merge { entries: BTreeMap::new() },
    )
}

fn set_branch(srv: &Server, name: &str, head: &str) {
    let path = srv.tmp.path().join(format!("branches/{name}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let value = serde_json::json!({
        "name": name, "parent": null, "head_op": head, "merges": [], "created_at": 0,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// Named ops of a DAG with two branches, two merges of `feature` into
/// `main`, and a third, independent root history merged into `main`:
///
/// ```text
/// g ─ a1 ─ a2 ─ a3 ─ m1 ─ a4 ─ a5 ─ m2 ─ a6 ─ m3 ─ a7      main
///       └─ b1 ─ b2 ─ b3 ┘          │         │
///                    └─ b4 ─ b5 ───┘         │
///                              └─ b6          │            feature (b6)
///                        r0 ─ r1 ─ r2 ────────┘            (independent root)
/// ```
struct Dag {
    ids: BTreeMap<&'static str, String>,
}

impl Dag {
    fn id(&self, name: &str) -> &str {
        &self.ids[name]
    }
}

fn seed_dag(srv: &Server) -> Dag {
    let mut ids: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut recs: Vec<OperationRecord> = Vec::new();
    let mut push = |name: &'static str, rec: OperationRecord, ids: &mut BTreeMap<_, _>| {
        ids.insert(name, rec.op_id.clone());
        recs.push(rec);
    };
    push("g", add(&[], "g"), &mut ids);
    for (n, p) in [("a1", "g"), ("a2", "a1"), ("a3", "a2"), ("b1", "a1"), ("b2", "b1"), ("b3", "b2")] {
        let parent = ids[p].clone();
        push(n, add(&[&parent], n), &mut ids);
    }
    let m1 = merge(&ids["a3"], &ids["b3"], 1);
    push("m1", m1, &mut ids);
    for (n, p) in [("a4", "m1"), ("a5", "a4"), ("b4", "b3"), ("b5", "b4")] {
        let parent = ids[p].clone();
        push(n, add(&[&parent], n), &mut ids);
    }
    let m2 = merge(&ids["a5"], &ids["b5"], 2);
    push("m2", m2, &mut ids);
    let (a6p, b6p) = (ids["m2"].clone(), ids["b5"].clone());
    push("a6", add(&[&a6p], "a6"), &mut ids);
    push("b6", add(&[&b6p], "b6"), &mut ids);
    push("r0", add(&[], "r0"), &mut ids);
    for (n, p) in [("r1", "r0"), ("r2", "r1")] {
        let parent = ids[p].clone();
        push(n, add(&[&parent], n), &mut ids);
    }
    let m3 = merge(&ids["a6"], &ids["r2"], 3);
    push("m3", m3, &mut ids);
    let a7p = ids["m3"].clone();
    push("a7", add(&[&a7p], "a7"), &mut ids);

    assert_eq!(post(&srv.addr, "/v1/ops/batch", &serde_json::to_string(&recs).unwrap()), 200);
    set_branch(srv, "main", &ids["a7"]);
    set_branch(srv, "feature", &ids["b6"]);
    Dag { ids }
}

/// What the pre-#971 handler returned for one page, computed directly from
/// the op log: `ops_since(head, after)` newest-first, reversed, truncated.
fn oracle_page(root: &std::path::Path, head: &str, after: Option<&str>, limit: usize) -> Vec<String> {
    let log = OpLog::open(root).unwrap();
    let after = after.map(String::from);
    let mut ops = log.ops_since(&head.to_string(), after.as_ref()).unwrap();
    ops.reverse();
    ops.truncate(limit);
    ops.into_iter().map(|r| r.op_id).collect()
}

fn ids_of(body: &str) -> Vec<String> {
    let recs: Vec<OperationRecord> = serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"));
    recs.into_iter().map(|r| r.op_id).collect()
}

fn since_path(branch: &str, after: Option<&str>, limit: Option<usize>) -> String {
    let mut p = format!("/v1/ops/since?branch={branch}");
    if let Some(a) = after {
        p.push_str(&format!("&after={a}"));
    }
    if let Some(n) = limit {
        p.push_str(&format!("&limit={n}"));
    }
    p
}

/// Drive the legacy client loop (`after=<last op of previous page>`) and
/// return every page it received. `addr_for(page_no)` picks the server per
/// page, so a test can restart the server mid-pull.
fn legacy_pull_via(
    addr_for: &mut dyn FnMut(usize) -> SocketAddr,
    branch: &str,
    start: Option<&str>,
    limit: usize,
) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor = start.map(String::from);
    loop {
        let addr = addr_for(pages.len());
        let (status, _, body) = get(&addr, &since_path(branch, cursor.as_deref(), Some(limit)));
        assert_eq!(status, 200, "{body}");
        let page = ids_of(&body);
        if page.is_empty() {
            break;
        }
        cursor = page.last().cloned();
        let short = page.len() < limit;
        pages.push(page);
        if short || pages.len() > 500 {
            break;
        }
    }
    pages
}

fn legacy_pull(srv: &Server, branch: &str, start: Option<&str>, limit: usize) -> Vec<Vec<String>> {
    legacy_pull_via(&mut |_| srv.addr, branch, start, limit)
}

/// The pre-#971 legacy loop, answered by the old rule (`ops_since(head,
/// after)` reversed, truncated), straight from the op log.
fn old_rule_legacy_pull(root: &std::path::Path, head: &str, start: Option<&str>, limit: usize) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let mut cursor = start.map(String::from);
    loop {
        let page = oracle_page(root, head, cursor.as_deref(), limit);
        if page.is_empty() {
            break;
        }
        cursor = page.last().cloned();
        let short = page.len() < limit;
        pages.push(page);
        if short || pages.len() > 500 {
            break;
        }
    }
    pages
}

/// Drive the cursor protocol as `lex op pull` does: follow
/// `X-Lex-Next-Cursor` until it's absent, sending `after=<last op>` too.
fn cursor_pull(srv: &Server, branch: &str, start: Option<&str>, limit: usize) -> Vec<Vec<String>> {
    let mut pages: Vec<Vec<String>> = Vec::new();
    let (status, headers, body) = get(&srv.addr, &since_path(branch, start, Some(limit)));
    assert_eq!(status, 200, "{body}");
    pages.push(ids_of(&body));
    let mut next = headers.get("x-lex-next-cursor").cloned();
    while let Some(c) = next {
        let after = pages.last().and_then(|p| p.last()).cloned();
        let (status, headers, body) =
            get(&srv.addr, &format!("{}&cursor={c}", since_path(branch, after.as_deref(), Some(limit))));
        assert_eq!(status, 200, "{body}");
        pages.push(ids_of(&body));
        next = headers.get("x-lex-next-cursor").cloned();
        assert!(pages.len() < 500, "cursor pull did not terminate");
    }
    pages.retain(|p| !p.is_empty());
    pages
}

/// The whole delta in one paged response: the canonical linearization.
fn canonical(srv: &Server, branch: &str, start: Option<&str>) -> Vec<String> {
    let (status, headers, body) = get(&srv.addr, &since_path(branch, start, Some(1_000_000)));
    assert_eq!(status, 200, "{body}");
    assert!(!headers.contains_key("x-lex-next-cursor"));
    ids_of(&body)
}

/// `got` is exactly the delta `head` minus `start`'s ancestry: every op
/// once, nothing else, and parents before children.
fn assert_exact_delta(root: &std::path::Path, head: &str, start: Option<&str>, got: &[String], ctx: &str) {
    let want: BTreeSet<String> = oracle_page(root, head, start, usize::MAX).into_iter().collect();
    let seen: BTreeSet<String> = got.iter().cloned().collect();
    assert_eq!(seen.len(), got.len(), "duplicates delivered ({ctx})");
    let missing: Vec<&String> = want.difference(&seen).collect();
    let extra: Vec<&String> = seen.difference(&want).collect();
    assert!(missing.is_empty() && extra.is_empty(), "missing {missing:?}, extra {extra:?} ({ctx})");
    assert_parents_first(root, got, ctx);
}

fn assert_parents_first(root: &std::path::Path, got: &[String], ctx: &str) {
    let log = OpLog::open(root).unwrap();
    let rank: BTreeMap<&String, usize> = got.iter().enumerate().map(|(i, id)| (id, i)).collect();
    for (i, id) in got.iter().enumerate() {
        for p in &log.get(id).unwrap().unwrap().op.parents {
            if let Some(&pr) = rank.get(p) {
                assert!(pr < i, "op {id} delivered before its parent {p} ({ctx})");
            }
        }
    }
}

fn starts(dag: &Dag) -> Vec<Option<String>> {
    let mut v: Vec<Option<String>> = vec![None];
    for n in ["g", "a2", "b2", "m1", "b5", "r1", "m3", "a7", "b6"] {
        v.push(Some(dag.id(n).to_string()));
    }
    // A cutoff the server has never seen (a client ahead on its own history).
    v.push(Some("0".repeat(64)));
    v
}

#[test]
fn legacy_after_paging_delivers_every_op_exactly_once() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    for (branch, head) in [("main", dag.id("a7")), ("feature", dag.id("b6"))] {
        for start in starts(&dag) {
            let canon = canonical(&srv, branch, start.as_deref());
            assert_exact_delta(srv.tmp.path(), head, start.as_deref(), &canon, "canonical");
            for limit in [1, 2, 3, 5, 8, 100] {
                let ctx = format!("branch={branch} start={start:?} limit={limit}");
                let pages = legacy_pull(&srv, branch, start.as_deref(), limit);
                for p in pages.iter().rev().skip(1) {
                    assert_eq!(p.len(), limit, "only the last page is short ({ctx})");
                }
                let flat = pages.concat();
                assert_exact_delta(srv.tmp.path(), head, start.as_deref(), &flat, &ctx);
                assert_eq!(flat, canon, "legacy paging walks the canonical order ({ctx})");
            }
        }
    }
}

/// A linear history (no merges) pages exactly as it always did: the
/// canonical order of an already-topological delta is the old order.
#[test]
fn a_linear_history_pages_exactly_as_before() {
    let srv = start_server();
    let mut recs = vec![add(&[], "l0")];
    for i in 1..40 {
        let parent = recs.last().unwrap().op_id.clone();
        recs.push(add(&[&parent], &format!("l{i}")));
    }
    assert_eq!(post(&srv.addr, "/v1/ops/batch", &serde_json::to_string(&recs).unwrap()), 200);
    let head = recs.last().unwrap().op_id.clone();
    set_branch(&srv, "main", &head);
    for start in [None, Some(recs[7].op_id.as_str())] {
        for limit in [1, 3, 7, 100] {
            let got = legacy_pull(&srv, "main", start, limit);
            let want = old_rule_legacy_pull(srv.tmp.path(), &head, start, limit);
            assert_eq!(got, want, "start={start:?} limit={limit}");
        }
    }
}

fn fresh_server(root: &std::path::Path) -> SocketAddr {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let state = Arc::new(State::open(root.to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    addr
}

/// Page ends persist under the store root, so a legacy pull that spans
/// server restarts (a fresh `State` for every page) is still exactly once.
#[test]
fn legacy_paging_across_server_restarts_is_still_exactly_once() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    let root = srv.tmp.path().to_path_buf();
    for (branch, head) in [("main", dag.id("a7")), ("feature", dag.id("b6"))] {
        for start in starts(&dag) {
            for limit in [1, 2, 3, 5] {
                let ctx = format!("branch={branch} start={start:?} limit={limit}");
                let flat = legacy_pull_via(&mut |_| fresh_server(&root), branch, start.as_deref(), limit).concat();
                assert_exact_delta(&root, head, start.as_deref(), &flat, &ctx);
            }
        }
    }
}

/// If the server has lost a pull's page end (evicted, or an unwritable
/// store root), that one page falls back to the ancestry reading of
/// `after`. The order is topological, so this can re-send ops but never
/// drops one, and the pull then continues from recorded page ends again
/// and terminates.
#[test]
fn a_forgotten_page_end_costs_duplicates_never_ops() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    let root = srv.tmp.path().to_path_buf();
    let conts = root.join("ops_since_continuations.json");
    for (branch, head) in [("main", dag.id("a7")), ("feature", dag.id("b6"))] {
        for start in starts(&dag) {
            for limit in [1, 2, 3, 5] {
                for forget_at in [1, 2, 4] {
                    let ctx = format!("branch={branch} start={start:?} limit={limit} forget_at={forget_at}");
                    let mut addr = fresh_server(&root);
                    let mut pick = |page: usize| {
                        if page == forget_at {
                            let _ = std::fs::remove_file(&conts);
                            addr = fresh_server(&root);
                        }
                        addr
                    };
                    let pages = legacy_pull_via(&mut pick, branch, start.as_deref(), limit);
                    assert!(pages.len() < 500, "pull did not terminate ({ctx})");
                    let flat = pages.concat();
                    let want: BTreeSet<String> =
                        oracle_page(&root, head, start.as_deref(), usize::MAX).into_iter().collect();
                    let seen: BTreeSet<String> = flat.iter().cloned().collect();
                    let missing: Vec<&String> = want.difference(&seen).collect();
                    assert!(missing.is_empty(), "ops never delivered: {missing:?} ({ctx})");
                }
            }
        }
    }
}

#[test]
fn unpaged_responses_are_unchanged() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    for (branch, head) in [("main", dag.id("a7")), ("feature", dag.id("b6"))] {
        for start in starts(&dag) {
            let (status, headers, body) = get(&srv.addr, &since_path(branch, start.as_deref(), None));
            assert_eq!(status, 200, "{body}");
            let want = oracle_page(srv.tmp.path(), head, start.as_deref(), usize::MAX);
            assert_eq!(ids_of(&body), want, "branch={branch} start={start:?}");
            assert!(!headers.contains_key("x-lex-next-cursor"), "unpaged response has no next page");
        }
    }
}

#[test]
fn cursor_paging_delivers_the_canonical_sequence() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    for (branch, head) in [("main", dag.id("a7")), ("feature", dag.id("b6"))] {
        for start in starts(&dag) {
            let canon = canonical(&srv, branch, start.as_deref());
            for limit in [1, 2, 3, 5, 8, 100] {
                let ctx = format!("branch={branch} start={start:?} limit={limit}");
                let pages = cursor_pull(&srv, branch, start.as_deref(), limit);
                for p in pages.iter().rev().skip(1) {
                    assert_eq!(p.len(), limit, "{ctx}");
                }
                let flat = pages.concat();
                assert_eq!(flat, canon, "{ctx}");
                assert_exact_delta(srv.tmp.path(), head, start.as_deref(), &flat, &ctx);
            }
        }
    }
}

/// A client whose genuine cutoff is some op X must get "ops outside X's
/// ancestry", even when another pull's page happened to end at X. Only a
/// page end with no competing reading is resumed by position, and a
/// cursor-following client's page ends are dropped as soon as it sends its
/// cursor.
#[test]
fn a_cursor_pull_leaves_no_page_end_behind() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    let _ = cursor_pull(&srv, "main", None, 3);
    let conts = std::fs::read_to_string(srv.tmp.path().join("ops_since_continuations.json")).unwrap_or_default();
    assert!(conts.trim().is_empty() || conts.trim() == "[]", "leftover continuations: {conts}");
    // So a later first page from any of those page ends reads as a cutoff.
    for start in starts(&dag) {
        let got = legacy_pull(&srv, "main", start.as_deref(), 3).concat();
        assert_exact_delta(srv.tmp.path(), dag.id("a7"), start.as_deref(), &got, &format!("start={start:?}"));
    }
}

#[test]
fn next_cursor_is_only_sent_when_ops_remain() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    let total = oracle_page(srv.tmp.path(), dag.id("a7"), None, usize::MAX).len();
    let (_, h, _) = get(&srv.addr, &since_path("main", None, Some(total)));
    assert!(!h.contains_key("x-lex-next-cursor"), "exactly-full last page: nothing remains");
    let (_, h, _) = get(&srv.addr, &since_path("main", None, Some(total - 1)));
    assert!(h.contains_key("x-lex-next-cursor"), "one op remains");
    let (_, h, _) = get(&srv.addr, &since_path("main", Some(dag.id("a7")), Some(5)));
    assert!(!h.contains_key("x-lex-next-cursor"), "caller at head");
    let (_, h, _) = get(&srv.addr, &since_path("nope", None, Some(5)));
    assert!(!h.contains_key("x-lex-next-cursor"), "unknown branch");
}

#[test]
fn a_cursor_pins_the_head_the_pull_started_from() {
    let srv = start_server();
    let dag = seed_dag(&srv);
    let want = canonical(&srv, "main", None);
    let (_, headers, body) = get(&srv.addr, &since_path("main", None, Some(4)));
    let mut got = ids_of(&body);
    let mut next = headers.get("x-lex-next-cursor").cloned();
    // `main` moves mid-pull (here: to feature's head). The pull finishes
    // the snapshot it started instead of splicing two histories.
    set_branch(&srv, "main", dag.id("b6"));
    while let Some(c) = next {
        let (status, h, body) = get(&srv.addr, &format!("/v1/ops/since?branch=main&limit=4&cursor={c}"));
        assert_eq!(status, 200, "{body}");
        got.extend(ids_of(&body));
        next = h.get("x-lex-next-cursor").cloned();
    }
    assert_eq!(got, want);
}

#[test]
fn a_malformed_or_unknown_cursor_is_a_400_not_an_empty_page() {
    let srv = start_server();
    let _dag = seed_dag(&srv);
    let unknown = format!("v1.{}._.3", "f".repeat(64));
    for c in ["garbage", "v1.abc", "v1.../../x._.0", "v1.ab.cd.notanumber", unknown.as_str()] {
        let (status, _, body) = get(&srv.addr, &format!("/v1/ops/since?branch=main&limit=2&cursor={c}"));
        assert_eq!(status, 400, "cursor `{c}` must be refused, got {status}: {body}");
    }
}

/// The #971 fix, end to end. A main line with a feature line merged back
/// every 10 ops. The pre-#971 rule, re-deriving each page from the last
/// op's ancestry, both repeats and drops ops here. The server now delivers
/// every op exactly once over the same unchanged legacy protocol, and the
/// cursor protocol delivers the same sequence.
#[test]
fn legacy_paging_is_lossless_where_the_old_rule_was_not() {
    let srv = start_server();
    let g = add(&[], "g");
    let (mut main, mut feature) = (g.op_id.clone(), g.op_id.clone());
    let mut recs = vec![g];
    for i in 1..300 {
        let tag = format!("x{i}");
        if i % 10 == 0 {
            let m = merge(&main, &feature, i);
            main = m.op_id.clone();
            feature = m.op_id.clone();
            recs.push(m);
        } else if i % 3 == 0 {
            let f = add(&[&feature], &tag);
            feature = f.op_id.clone();
            recs.push(f);
        } else {
            let a = add(&[&main], &tag);
            main = a.op_id.clone();
            recs.push(a);
        }
    }
    assert_eq!(post(&srv.addr, "/v1/ops/batch", &serde_json::to_string(&recs).unwrap()), 200);
    set_branch(&srv, "main", &main);
    let root = srv.tmp.path();

    let old: Vec<String> = old_rule_legacy_pull(root, &main, None, 25).concat();
    let old_distinct: BTreeSet<&String> = old.iter().collect();
    let full = oracle_page(root, &main, None, usize::MAX);
    assert!(
        old_distinct.len() < full.len() && old_distinct.len() < old.len(),
        "fixture no longer exercises the old loss: {} received, {} distinct, {} in the delta",
        old.len(),
        old_distinct.len(),
        full.len()
    );

    let legacy = legacy_pull(&srv, "main", None, 25).concat();
    assert_exact_delta(root, &main, None, &legacy, "legacy");
    assert_eq!(cursor_pull(&srv, "main", None, 25).concat(), legacy);
    assert_eq!(legacy.last(), Some(&main), "the head is the last op delivered");
}


