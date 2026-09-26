//! #1062: a merged head must be a function of the resolved merge, not of the
//! op ids (and so the replay order) of the two histories it joins.
//!
//! The issue's exact scenario, driven through the CLI: a single-file store,
//! `feature` renames `add` to `plus` while `main` modifies `add`'s body, so the
//! merge reports one `delete_modify` conflict on the old sig. It is resolved
//! `take_ours` (keep main's modified `add`) and committed. Every run uses a
//! fresh store, so the intent timestamps, and therefore the op ids, differ per
//! run; the merged head must not.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn publish(store: &Path, src: &Path) {
    let out = Command::new(lex_bin())
        .args(["--output", "json", "publish", "--store", store.to_str().unwrap(), src.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "publish failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn lex_json(args: &[&str]) -> serde_json::Value {
    let out = Command::new(lex_bin()).args(["--output", "json"]).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "lex {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

struct Setup {
    store: std::path::PathBuf,
    add_sig: String,
    /// The sig `feature` introduced by renaming `add` (`None` when `feature`
    /// simply deleted `add`).
    plus_sig: Option<String>,
}

/// What `feature` does to `add`.
#[derive(Clone, Copy)]
enum FeatureEdit {
    /// The issue's scenario: rename it to `plus`.
    Rename,
    /// Delete it. The same `delete_modify` conflict, with no replacement sig.
    Delete,
}

/// Build the issue's two diverged branches in a fresh store under `dir`:
/// `feature` renamed (or deleted) `add`, `main` modified `add`'s body.
fn setup(dir: &Path, edit: FeatureEdit) -> Setup {
    let store = dir.join("store");
    let src = dir.join("a.lex");

    std::fs::write(&src, "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn keep(n :: Int) -> Int { n }\n").unwrap();
    publish(&store, &src);

    let s = lex_store::Store::open(&store).unwrap();
    s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
    s.set_current_branch("feature").unwrap();
    drop(s);
    let feature_src = match edit {
        FeatureEdit::Rename => "fn plus(x :: Int, y :: Int) -> Int { x + y }\nfn keep(n :: Int) -> Int { n }\n",
        FeatureEdit::Delete => "fn keep(n :: Int) -> Int { n }\n",
    };
    std::fs::write(&src, feature_src).unwrap();
    publish(&store, &src);

    let s = lex_store::Store::open(&store).unwrap();
    s.set_current_branch(lex_store::DEFAULT_BRANCH).unwrap();
    drop(s);
    // main modifies add's body.
    std::fs::write(&src, "fn add(x :: Int, y :: Int) -> Int { x + y + 1 }\nfn keep(n :: Int) -> Int { n }\n").unwrap();
    publish(&store, &src);

    let s = lex_store::Store::open(&store).unwrap();
    let main_before = s.branch_head(lex_store::DEFAULT_BRANCH).unwrap();
    let feat = s.branch_head("feature").unwrap();
    drop(s);
    let add_sig_main = main_before
        .keys()
        .find(|k| !feat.contains_key(*k))
        .expect("main's `add` sig must be absent from feature (renamed away)")
        .clone();
    let plus_sig = feat.keys().find(|k| !main_before.contains_key(*k)).cloned();
    assert_eq!(plus_sig.is_some(), matches!(edit, FeatureEdit::Rename));

    Setup { store, add_sig: add_sig_main, plus_sig }
}

/// `merge start` / `resolve` / `commit` with one resolution for the one
/// conflict; returns the merged head's sig->stage map.
fn merge_with(setup: &Setup, resolution: &str) -> BTreeMap<String, String> {
    let (store, add_sig_main) = (&setup.store, &setup.add_sig);
    let st = store.to_str().unwrap().to_string();
    let tmp = store.parent().unwrap();
    let v = lex_json(&["merge", "start", "--store", &st, "--src", "feature", "--dst", lex_store::DEFAULT_BRANCH]);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();
    let conflicts = v.pointer("/data/conflicts").unwrap().as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "expected exactly one conflict: {conflicts:?}");
    assert_eq!(conflicts[0]["kind"], "delete_modify");
    let conflict_id = conflicts[0]["conflict_id"].as_str().unwrap().to_string();
    assert_eq!(&conflict_id, add_sig_main);

    let res = tmp.join("res.json");
    std::fs::write(
        &res,
        serde_json::to_vec(&serde_json::json!([{"conflict_id": conflict_id, "resolution": {"kind": resolution}}])).unwrap(),
    )
    .unwrap();
    lex_json(&["merge", "resolve", "--store", &st, &merge_id, "--file", res.to_str().unwrap()]);
    lex_json(&["merge", "commit", "--store", &st, &merge_id]);

    lex_store::Store::open(store).unwrap().branch_head(lex_store::DEFAULT_BRANCH).unwrap()
}

/// Run the issue scenario once against a fresh store and return `(merged head
/// sig->stage map, sig of the modified-on-main `add`, sig of `plus`)`.
fn run_scenario(resolution: &str) -> (BTreeMap<String, String>, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let s = setup(tmp.path(), FeatureEdit::Rename);
    let head = merge_with(&s, resolution);
    (head, s.add_sig, s.plus_sig.unwrap())
}

#[test]
fn issue_scenario_take_ours_keeps_the_modified_add_every_time() {
    let mut shapes = Vec::new();
    for i in 0..20 {
        let (head, add_sig, plus_sig) = run_scenario("take_ours");
        assert!(head.contains_key(&add_sig), "run {i}: take_ours lost main's modified `add`: {head:?}");
        assert!(head.contains_key(&plus_sig), "run {i}: feature's `plus` missing: {head:?}");
        assert_eq!(head.len(), 3, "run {i}: expected add, plus, keep: {head:?}");
        // Sig ids are content hashes: identical across runs. Compare the shape.
        shapes.push(head.into_iter().collect::<Vec<_>>());
    }
    for (i, s) in shapes.iter().enumerate() {
        assert_eq!(s, &shapes[0], "run {i} produced a different merged head than run 0");
    }
}

#[test]
fn issue_scenario_take_theirs_takes_the_rename_every_time() {
    for i in 0..5 {
        let (head, add_sig, plus_sig) = run_scenario("take_theirs");
        assert!(!head.contains_key(&add_sig), "run {i}: take_theirs kept the retired `add`: {head:?}");
        assert!(head.contains_key(&plus_sig), "run {i}: `plus` missing: {head:?}");
        assert_eq!(head.len(), 2, "run {i}: expected plus + keep: {head:?}");
    }
}

// ---- client == hub == pulled store ----------------------------------------

mod hub {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use lex_api::handlers::State;

    pub fn start() -> (SocketAddr, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
        let addr: SocketAddr = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr,
            _ => panic!("expected IP listener"),
        };
        let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
        thread::spawn(move || lex_api::serve_on(server, state));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        loop {
            if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                s.set_read_timeout(Some(Duration::from_millis(200))).ok();
                let mut buf = [0u8; 16];
                if s.write_all(probe).is_ok() && s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "test server never became ready");
            thread::sleep(Duration::from_millis(20));
        }
        (addr, tmp)
    }
}

fn lex_in(home: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(lex_bin())
        .current_dir(home)
        .env("HOME", home)
        .env_remove("LEX_STORE")
        .env_remove("LEXHUB_TOKEN")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "lex {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git_tree(dir: &Path) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "HEAD^{tree}"]).output().unwrap();
    assert!(out.status.success(), "git rev-parse: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The merged op's head must be the same wherever it is computed: on the
/// client, on the hub that received it by push (whose branch already had a
/// head snapshot at the pre-merge tip, so it takes the incremental path), and
/// in a fresh store that pulled it (a full replay) — and every one of them
/// must export to the same git tree.
#[test]
fn merged_head_is_identical_on_client_hub_and_pulled_store() {
    for (edit, resolution) in [
        (FeatureEdit::Rename, "take_ours"),
        (FeatureEdit::Rename, "take_theirs"),
        (FeatureEdit::Delete, "take_ours"),
        (FeatureEdit::Delete, "take_theirs"),
    ] {
        let (addr, hub_tmp) = hub::start();
        let hub_url = format!("http://{addr}");
        let work = tempfile::tempdir().unwrap();
        let s = setup(work.path(), edit);
        let client_store = s.store.to_str().unwrap().to_string();

        // The hub learns main's pre-merge head first, so its branch_head
        // snapshot sits at the dst tip when the merge arrives.
        lex_in(work.path(), &["op", "push", &hub_url, "--store", &client_store]);

        let client_head = merge_with(&s, resolution);
        lex_in(work.path(), &["op", "push", &hub_url, "--store", &client_store]);

        let client = lex_store::Store::open(&s.store).unwrap();
        let head_op = client.get_branch(lex_store::DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();

        let hub_store = lex_store::Store::open(hub_tmp.path()).unwrap();
        let hub_branch = hub_store.get_branch(lex_store::DEFAULT_BRANCH).unwrap().unwrap();
        assert_eq!(hub_branch.head_op.as_deref(), Some(head_op.as_str()), "{resolution}: hub is at the merge op");
        assert_eq!(hub_store.branch_head(lex_store::DEFAULT_BRANCH).unwrap(), client_head, "{resolution}: hub != client");
        assert_eq!(hub_store.sig_map_at_op(&head_op).unwrap(), client_head, "{resolution}: hub full replay != client");

        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh_store = fresh_dir.path().join("store");
        lex_in(fresh_dir.path(), &["op", "pull", &hub_url, "--store", fresh_store.to_str().unwrap()]);
        let pulled = lex_store::Store::open(&fresh_store).unwrap();
        let pulled_head = pulled.get_branch(lex_store::DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
        assert_eq!(pulled_head, head_op, "{resolution}: pulled the same merge op");
        assert_eq!(pulled.branch_head(lex_store::DEFAULT_BRANCH).unwrap(), client_head, "{resolution}: pulled != client");

        // The resolution itself, so equality is not "equally wrong".
        assert_eq!(
            client_head.contains_key(&s.add_sig),
            resolution == "take_ours",
            "{resolution}: {client_head:?}"
        );
        let renamed = usize::from(matches!(edit, FeatureEdit::Rename));
        let want_len = renamed + usize::from(resolution == "take_ours") + 1; // + keep
        assert_eq!(client_head.len(), want_len, "{resolution}: {client_head:?}");

        // export-git from the client and from the pulled store: same tree.
        let out_a = tempfile::tempdir().unwrap();
        let out_b = tempfile::tempdir().unwrap();
        lex_in(work.path(), &["export-git", out_a.path().to_str().unwrap(), "--store", &client_store]);
        lex_in(fresh_dir.path(), &["export-git", out_b.path().to_str().unwrap(), "--store", fresh_store.to_str().unwrap()]);
        assert_eq!(git_tree(out_a.path()), git_tree(out_b.path()), "{resolution}: export-git trees differ");
    }
}
