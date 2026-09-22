//! `lex op pull` over a multi-page history (#971): the real binary against
//! a real `lex-api` server, with a history longer than one pull page
//! (1000 ops) that includes merges. The client must follow the server's
//! `X-Lex-Next-Cursor` and receive every op of the delta exactly once, in
//! the server's canonical (parents-first) order.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use lex_api::handlers::State;
use lex_vcs::{OpLog, Operation, OperationKind, OperationRecord, StageTransition};
use tempfile::TempDir;

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
    let t = StageTransition::Create { sig_id: format!("sig-{i}"), stage_id: format!("stg-{i}") };
    OperationRecord::new(Operation::new(kind, parents), t)
}

/// A main line with a side line forked off it and merged back every 40 ops.
/// Write a record the way `OpLog::put` lays it out, minus the per-file
/// fsync (which would dominate seeding thousands of ops).
struct Log<'a>(&'a std::path::Path);
impl Log<'_> {
    fn put(&self, rec: &OperationRecord) -> std::io::Result<()> {
        std::fs::write(self.0.join(format!("ops/{}.json", rec.op_id)), serde_json::to_vec(rec).unwrap())
    }
}

fn seed(root: &std::path::Path, n: usize) -> String {
    OpLog::open(root).unwrap();
    let log = Log(root);
    let g = op(vec![], 0);
    log.put(&g).unwrap();
    let (mut main, mut side) = (g.op_id.clone(), g.op_id);
    for i in 1..n {
        let rec = if i % 40 == 0 {
            op(vec![main.clone(), side.clone()], i)
        } else if i % 3 == 0 {
            let r = op(vec![side.clone()], i);
            side = r.op_id.clone();
            log.put(&r).unwrap();
            continue;
        } else {
            op(vec![main.clone()], i)
        };
        log.put(&rec).unwrap();
        main = rec.op_id.clone();
        if i % 40 == 0 {
            side = main.clone();
        }
    }
    let path = root.join("branches/main.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let v = serde_json::json!({"name":"main","parent":null,"head_op":main,"merges":[],"created_at":0});
    std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    main
}

#[test]
fn op_pull_follows_the_cursor_across_pages() {
    let remote = TempDir::new().unwrap();
    let head = seed(remote.path(), 2500);
    let mut want: Vec<String> = OpLog::open(remote.path())
        .unwrap()
        .ops_since(&head, None)
        .unwrap()
        .into_iter()
        .map(|r| r.op_id)
        .collect();
    want.reverse();

    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let state = Arc::new(State::open(remote.path().to_path_buf()).unwrap());
    let urls: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&urls);
    thread::spawn(move || {
        for req in server.incoming_requests() {
            seen.lock().unwrap().push(req.url().to_string());
            let _ = lex_api::handlers::handle(Arc::clone(&state), req);
        }
    });

    let local = TempDir::new().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lex"))
        .args(["--output", "json", "op", "pull", &format!("http://127.0.0.1:{port}"), "--dry-run"])
        .args(["--store", local.path().to_str().unwrap()])
        .env_remove("LEXHUB_TOKEN")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let got: Vec<String> = v["data"]["op_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("no op_ids in {v}"))
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    assert!(want.len() > 2000, "fixture must span three pages");
    // Every op of the delta exactly once, parents before children, head last.
    let log = OpLog::open(remote.path()).unwrap();
    let distinct: BTreeSet<&String> = got.iter().collect();
    assert_eq!(distinct.len(), got.len(), "duplicates");
    assert_eq!(distinct, want.iter().collect::<BTreeSet<_>>());
    let rank: std::collections::HashMap<&String, usize> = got.iter().enumerate().map(|(i, id)| (id, i)).collect();
    for (i, id) in got.iter().enumerate() {
        for p in &log.get(id).unwrap().unwrap().op.parents {
            assert!(rank[p] < i, "{id} before its parent {p}");
        }
    }
    assert_eq!(got.last(), Some(&head));

    let pages: Vec<String> =
        urls.lock().unwrap().iter().filter(|u| u.starts_with("/v1/ops/since")).cloned().collect();
    assert_eq!(pages.len(), 3, "2500 ops in pages of 1000: {pages:?}");
    assert!(!pages[0].contains("cursor="));
    assert!(pages[1..].iter().all(|u| u.contains("cursor=") && u.contains("after=")), "{pages:?}");
}
