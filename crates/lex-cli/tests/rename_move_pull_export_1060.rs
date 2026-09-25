//! #1060: a history containing a renamed function or a moved `.lex` file must
//! survive `publish -> push -> pull -> export-git` through a real in-process
//! `lex-api` hub.
//!
//! A rename changes the sig but not the StageId (#826), so the pre-rename and
//! post-rename declarations share one StageId under two sigs holding two
//! ASTs. `op pull` chose what to fetch per `(sig, stage)` pair but fetched by
//! bare id, so the hub answered with the pre-rename variant, the puller filed
//! it under the old sig, and the pulled head named `(new sig, stage)` — a pair
//! nothing had supplied. `export-git` on that store failed with
//! "unsatisfiable head entry ... (#992)". Nothing earlier noticed: publish,
//! push and pull all succeeded.
//!
//! A moved file is the same bug (the mangling prefix is path-derived, so a move
//! renames every declaration in the file), plus a second defect: the rename op
//! did not say which file the declaration moved to, so even the origin store
//! exported it back into the old path.
//!
//! Every git-touching step here runs through `lex export-git`, which pins its
//! own author/committer identity; no test commits with an ambient one.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── the real in-process lex-api hub (files-v1 capable) ──────────────────────
// Copied from `op_push_pull_files_1007.rs`'s established pattern.

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

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

use std::collections::BTreeMap;

fn w(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A package dir with its own store, publishing into a hub.
struct Pkg {
    dir: PathBuf,
    store: String,
    env_root: PathBuf,
}

impl Pkg {
    fn new(root: &Path, env_root: &Path, name: &str, files: &[(&str, &str)]) -> Pkg {
        let dir = root.join(name);
        w(&dir, "lex.toml", &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"));
        for (p, b) in files {
            w(&dir, p, b);
        }
        let store = dir.join(".lex/store").to_string_lossy().into_owned();
        Pkg { dir, store, env_root: env_root.to_path_buf() }
    }

    fn publish(&self) {
        ok(&self.dir, &self.env_root, &["publish", "--store", &self.store, "--activate", "."]);
    }

    fn push(&self, hub: &str) {
        ok(&self.dir, &self.env_root, &["op", "push", hub, "--store", &self.store]);
    }

    /// Replace the working tree's `src/` with `files` (dropping `remove`).
    fn edit(&self, remove: &[&str], files: &[(&str, &str)]) {
        for p in remove {
            std::fs::remove_file(self.dir.join(p)).unwrap();
        }
        for (p, b) in files {
            w(&self.dir, p, b);
        }
    }
}

fn pull_into(hub: &str, env_root: &Path, at: &Path, extra: &[&str]) -> (String, Output) {
    std::fs::create_dir_all(at).unwrap();
    let store = at.join(".lex/store").to_string_lossy().into_owned();
    let mut args = vec!["op", "pull", hub, "--store", &store];
    args.extend_from_slice(extra);
    let out = run_lex(at, env_root, &args);
    (store, out)
}

fn export(cwd: &Path, env_root: &Path, store: &str) -> (Output, TempDir) {
    let out = TempDir::new().unwrap();
    let res = run_lex(cwd, env_root, &["export-git", out.path().to_str().unwrap(), "--store", store]);
    (res, out)
}

fn export_ok(cwd: &Path, env_root: &Path, store: &str) -> TempDir {
    let (res, out) = export(cwd, env_root, store);
    assert!(
        res.status.success(),
        "export-git failed: {}",
        String::from_utf8_lossy(&res.stderr)
    );
    out
}

/// The exported repo's `.lex` files and manifest, path -> contents.
fn lex_tree(repo: &Path) -> BTreeMap<String, String> {
    let listing = Command::new("git").arg("-C").arg(repo).args(["ls-files"]).output().unwrap();
    assert!(listing.status.success());
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter(|p| p.ends_with(".lex"))
        .map(|p| (p.to_string(), std::fs::read_to_string(repo.join(p)).unwrap()))
        .collect()
}

/// What a fresh publish of `files` exports — the tree a history that arrives at
/// the same source must reproduce.
fn control_tree(root: &Path, env_root: &Path, files: &[(&str, &str)]) -> BTreeMap<String, String> {
    let pkg = Pkg::new(root, env_root, "control", files);
    pkg.publish();
    lex_tree(export_ok(&pkg.dir, env_root, &pkg.store).path())
}

const ADD: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
const PLUS: &str = "fn plus(x :: Int, y :: Int) -> Int { x + y }\n";
const UTIL: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
const MAIN_V1: &str = "import \"./util\" as u\nfn main() -> Int { u.add(1, 2) }\n";
const MAIN_V2: &str = "import \"./lib/util\" as u\nfn main() -> Int { u.add(1, 2) }\n";

/// Publish `v1`, push, transform to `v2`, publish, push. Returns the origin
/// package and the hub.
struct Scenario {
    _server: Server,
    _hub_tmp: TempDir,
    hub: String,
    env_root: TempDir,
    work: TempDir,
    pkg: Pkg,
    /// The op that preceded the rename/move, for `op pull --since`.
    before_op: String,
}

fn scenario(v1: &[(&str, &str)], remove: &[&str], v2: &[(&str, &str)]) -> Scenario {
    let (server, hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let pkg = Pkg::new(work.path(), env_root.path(), "pkg", v1);
    pkg.publish();
    pkg.push(&hub);
    let before = json_ok(&pkg.dir, env_root.path(), &["op", "log", "--store", &pkg.store]);
    let before_op = op_ids(&before).remove(0);
    pkg.edit(remove, v2);
    pkg.publish();
    pkg.push(&hub);
    Scenario { _server: server, _hub_tmp: hub_tmp, hub, env_root, work, pkg, before_op }
}

/// Op ids, newest first, from `lex op log --output json`.
fn op_ids(v: &serde_json::Value) -> Vec<String> {
    let arr = data(v)["log"].as_array().cloned().unwrap_or_default();
    arr.iter().filter_map(|o| o["op_id"].as_str().map(String::from)).collect()
}

fn assert_shapes_agree(sc: &Scenario, control: &BTreeMap<String, String>) {
    let origin = lex_tree(export_ok(&sc.pkg.dir, sc.env_root.path(), &sc.pkg.store).path());
    assert_eq!(&origin, control, "the origin store's export must equal a fresh publish of the same source");

    let (store_b, pulled) = pull_into(&sc.hub, sc.env_root.path(), &sc.work.path().join("fresh"), &[]);
    assert!(pulled.status.success(), "pull failed: {}", String::from_utf8_lossy(&pulled.stderr));
    assert!(
        !String::from_utf8_lossy(&pulled.stderr).contains("not renderable"),
        "a pull from a current hub must not warn: {}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let fresh = sc.work.path().join("fresh");
    let exported = lex_tree(export_ok(&fresh, sc.env_root.path(), &store_b).path());
    assert_eq!(&exported, control, "the PULLED store's export must equal a fresh publish of the same source");
}

// ── 1. a plain function rename ──────────────────────────────────────────────

#[test]
fn a_renamed_function_survives_push_pull_export() {
    let sc = scenario(&[("src/main.lex", ADD)], &[], &[("src/main.lex", PLUS)]);
    let control_root = TempDir::new().unwrap();
    let control = control_tree(control_root.path(), sc.env_root.path(), &[("src/main.lex", PLUS)]);
    assert_eq!(control.len(), 1, "{control:?}");
    assert!(control.values().next().unwrap().contains("fn plus"), "{control:?}");
    assert_shapes_agree(&sc, &control);
}

// ── 2. a file move ──────────────────────────────────────────────────────────

#[test]
fn a_moved_file_survives_push_pull_export() {
    let sc = scenario(
        &[("src/main.lex", MAIN_V1), ("src/util.lex", UTIL)],
        &["src/util.lex"],
        &[("src/main.lex", MAIN_V2), ("src/lib/util.lex", UTIL)],
    );
    let control_root = TempDir::new().unwrap();
    let control = control_tree(
        control_root.path(),
        sc.env_root.path(),
        &[("src/main.lex", MAIN_V2), ("src/lib/util.lex", UTIL)],
    );
    let paths: Vec<&String> = control.keys().collect();
    assert_eq!(paths, ["src/lib/util.lex", "src/main.lex"], "control layout");
    assert_shapes_agree(&sc, &control);
}

// ── 3. a signature change in a multi-file package keeps its files ───────────

#[test]
fn a_signature_change_in_a_multi_file_package_keeps_its_files() {
    // `ModifyBody` with a `to_sig_id` (#992): the head moves to a new sig, and
    // the export's head tracker used to drop the declaration's file, collapsing
    // the package into a single `src.lex`.
    let main_v2 = "import \"./util\" as u\nfn main() -> Float { u.add(1.0, 2.0) }\n";
    let util_v2 = "fn add(x :: Float, y :: Float) -> Float { x + y }\n";
    let sc = scenario(
        &[("src/main.lex", MAIN_V1), ("src/util.lex", UTIL)],
        &[],
        &[("src/main.lex", main_v2), ("src/util.lex", util_v2)],
    );
    let control_root = TempDir::new().unwrap();
    let control = control_tree(
        control_root.path(),
        sc.env_root.path(),
        &[("src/main.lex", main_v2), ("src/util.lex", util_v2)],
    );
    let paths: Vec<&String> = control.keys().collect();
    assert_eq!(paths, ["src/main.lex", "src/util.lex"], "control layout");
    assert_shapes_agree(&sc, &control);
}

// ── 4. histories damaged by a pre-fix pull ──────────────────────────────────

/// Recreate what a pre-#1060 `op pull` left behind: the head's renamed sig
/// bound to a stage that is filed only under the pre-rename sig. Drops the
/// stage file for the declaration named `plus` from the pulled store.
fn damage_like_a_pre_fix_pull(store: &str) {
    let stages = Path::new(store).join("stages");
    let mut dropped = 0;
    for sig_dir in std::fs::read_dir(&stages).unwrap().flatten() {
        let impls = sig_dir.path().join("implementations");
        let Ok(rd) = std::fs::read_dir(&impls) else { continue };
        for f in rd.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".ast.json") {
                continue;
            }
            let ast: serde_json::Value = serde_json::from_slice(&std::fs::read(f.path()).unwrap()).unwrap();
            if ast["name"].as_str().is_some_and(|n| n.ends_with("plus")) {
                std::fs::remove_file(f.path()).unwrap();
                dropped += 1;
            }
        }
    }
    assert_eq!(dropped, 1, "expected exactly one `plus` stage file to drop");
}

#[test]
fn a_store_damaged_by_an_old_pull_fails_export_and_a_repull_repairs_it() {
    let sc = scenario(&[("src/main.lex", ADD)], &[], &[("src/main.lex", PLUS)]);
    let (store_b, pulled) = pull_into(&sc.hub, sc.env_root.path(), &sc.work.path().join("fresh"), &[]);
    assert!(pulled.status.success());
    let fresh = sc.work.path().join("fresh");
    damage_like_a_pre_fix_pull(&store_b);

    // The damage is exactly the reported failure.
    let (res, _) = export(&fresh, sc.env_root.path(), &store_b);
    assert!(!res.status.success(), "the damaged store must not export");
    assert!(String::from_utf8_lossy(&res.stderr).contains("unsatisfiable head entry"));

    // A plain re-pull sees nothing new, so it cannot help...
    let again = run_lex(&fresh, sc.env_root.path(), &["op", "pull", &sc.hub, "--store", &store_b]);
    assert!(again.status.success());
    assert!(!export(&fresh, sc.env_root.path(), &store_b).0.status.success());

    // ...but re-pulling from before the rename re-derives what is missing per
    // pair, and the store is whole again.
    let (_, out) = pull_into(&sc.hub, sc.env_root.path(), &fresh, &["--since", &sc.before_op]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let control_root = TempDir::new().unwrap();
    let control = control_tree(control_root.path(), sc.env_root.path(), &[("src/main.lex", PLUS)]);
    assert_eq!(lex_tree(export_ok(&fresh, sc.env_root.path(), &store_b).path()), control);
}

#[test]
fn a_republish_heals_a_store_damaged_by_an_old_pull() {
    // The #995 self-heal: a publish retires a head entry whose content is filed
    // under another sig, so a damaged clone that republishes is repaired without
    // any help from the hub.
    let sc = scenario(&[("src/main.lex", ADD)], &[], &[("src/main.lex", PLUS)]);
    let (store_b, pulled) = pull_into(&sc.hub, sc.env_root.path(), &sc.work.path().join("fresh"), &[]);
    assert!(pulled.status.success());
    let fresh = sc.work.path().join("fresh");
    damage_like_a_pre_fix_pull(&store_b);
    assert!(!export(&fresh, sc.env_root.path(), &store_b).0.status.success());

    // The clone's working tree is the package at the head it pulled.
    w(&fresh, "lex.toml", "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n");
    w(&fresh, "src/main.lex", PLUS);
    ok(&fresh, sc.env_root.path(), &["publish", "--store", &store_b, "--activate", "."]);

    let control_root = TempDir::new().unwrap();
    let control = control_tree(control_root.path(), sc.env_root.path(), &[("src/main.lex", PLUS)]);
    assert_eq!(lex_tree(export_ok(&fresh, sc.env_root.path(), &store_b).path()), control);
}

// ── 5. merges across a rename ───────────────────────────────────────────────

fn publish_file(store: &Path, env_root: &Path, src: &Path) {
    ok(src.parent().unwrap(), env_root, &["publish", "--store", store.to_str().unwrap(), src.to_str().unwrap()]);
}

fn on_branch(store: &Path, branch: &str) {
    let s = lex_store::Store::open(store).unwrap();
    s.set_current_branch(branch).unwrap();
}

fn merge_start(store: &Path, env_root: &Path, src: &str, dst: &str) -> serde_json::Value {
    json_ok(store, env_root, &["merge", "start", "--store", store.to_str().unwrap(), "--src", src, "--dst", dst])
}

#[test]
fn a_merge_of_a_rename_and_an_unrelated_edit_lands_a_satisfiable_head() {
    let env_root = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let src = dir.path().join("a.lex");
    let keep = "fn keep(n :: Int) -> Int { n + 100 }\n";
    std::fs::write(&src, format!("{ADD}{keep}")).unwrap();
    publish_file(&store, env_root.path(), &src);
    {
        let s = lex_store::Store::open(&store).unwrap();
        s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
        s.set_current_branch("feature").unwrap();
    }
    std::fs::write(&src, format!("{PLUS}{keep}")).unwrap();
    publish_file(&store, env_root.path(), &src);
    on_branch(&store, lex_store::DEFAULT_BRANCH);
    std::fs::write(&src, format!("{ADD}fn keep(n :: Int) -> Int {{ n + 200 }}\n")).unwrap();
    publish_file(&store, env_root.path(), &src);

    let started = merge_start(&store, env_root.path(), "feature", lex_store::DEFAULT_BRANCH);
    let conflicts = data(&started)["conflicts"].as_array().unwrap();
    assert!(conflicts.is_empty(), "disjoint edits must not conflict: {started}");
    let merge_id = data(&started)["merge_id"].as_str().unwrap().to_string();
    json_ok(&store, env_root.path(), &["merge", "commit", "--store", store.to_str().unwrap(), &merge_id]);

    let s = lex_store::Store::open(&store).unwrap();
    let head = s.branch_head(lex_store::DEFAULT_BRANCH).unwrap();
    s.check_pairs_satisfiable(&head).expect("no unsatisfiable pair on the merged head");
    let out = export_ok(&store, env_root.path(), store.to_str().unwrap());
    let rendered = lex_tree(out.path()).into_values().collect::<Vec<_>>().join("\n");
    assert!(rendered.contains("fn plus"), "the rename landed:\n{rendered}");
    assert!(!rendered.contains("fn add"), "the old name is gone:\n{rendered}");
    assert!(rendered.contains("n + 200"), "main's edit landed:\n{rendered}");
}

#[test]
fn a_merge_of_a_rename_and_an_edit_to_the_same_function_reports_the_same_conflict_as_before() {
    let env_root = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let src = dir.path().join("a.lex");
    std::fs::write(&src, ADD).unwrap();
    publish_file(&store, env_root.path(), &src);
    {
        let s = lex_store::Store::open(&store).unwrap();
        s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
        s.set_current_branch("feature").unwrap();
    }
    std::fs::write(&src, PLUS).unwrap();
    publish_file(&store, env_root.path(), &src);
    on_branch(&store, lex_store::DEFAULT_BRANCH);
    std::fs::write(&src, "fn add(x :: Int, y :: Int) -> Int { x + y + 1 }\n").unwrap();
    publish_file(&store, env_root.path(), &src);

    let started = merge_start(&store, env_root.path(), "feature", lex_store::DEFAULT_BRANCH);
    let conflicts = data(&started)["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{started}");
    assert_eq!(
        conflicts[0]["kind"], "delete_modify",
        "the rename retires the old sig, which the other branch modified: {started}"
    );
    let merge_id = data(&started)["merge_id"].as_str().unwrap().to_string();
    let conflict_id = conflicts[0]["conflict_id"].as_str().unwrap().to_string();

    let res = dir.path().join("res.json");
    std::fs::write(
        &res,
        serde_json::json!([{ "conflict_id": conflict_id, "resolution": { "kind": "take_ours" } }]).to_string(),
    )
    .unwrap();
    json_ok(&store, env_root.path(), &[
        "merge", "resolve", "--store", store.to_str().unwrap(), &merge_id, "--file", res.to_str().unwrap(),
    ]);
    json_ok(&store, env_root.path(), &["merge", "commit", "--store", store.to_str().unwrap(), &merge_id]);

    // What the merged head holds beyond the renamed declaration is out of scope
    // here: which of the two branches' sig entries survives depends on the order
    // the DAG is replayed in (#1062). What #1060 owns is that the
    // rename's half of it is sound: the head names no pair no store can hold,
    // and it still renders.
    let s = lex_store::Store::open(&store).unwrap();
    let head = s.branch_head(lex_store::DEFAULT_BRANCH).unwrap();
    s.check_pairs_satisfiable(&head).expect("no unsatisfiable pair on the merged head");
    let out = export_ok(&store, env_root.path(), store.to_str().unwrap());
    let rendered = lex_tree(out.path()).into_values().collect::<Vec<_>>().join("\n");
    assert!(rendered.contains("fn plus"), "the rename landed:\n{rendered}");
}

// ── 6. replay ───────────────────────────────────────────────────────────────

#[test]
fn a_rename_op_replays_and_reproduces() {
    let env_root = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let store = dir.path().join("store");
    let src = dir.path().join("a.lex");
    std::fs::write(&src, ADD).unwrap();
    publish_file(&store, env_root.path(), &src);
    std::fs::write(&src, PLUS).unwrap();
    publish_file(&store, env_root.path(), &src);

    let log = json_ok(dir.path(), env_root.path(), &["op", "log", "--store", store.to_str().unwrap()]);
    let rename = data(&log)["log"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op"] == "rename_symbol")
        .unwrap_or_else(|| panic!("the republish must be a rename, not remove+add: {log}"))["op_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The exact regeneration reproduces the rename.
    let cand = dir.path().join("cand.lex");
    std::fs::write(&cand, PLUS).unwrap();
    let r = json_ok(dir.path(), env_root.path(), &[
        "op", "replay", &rename, "--store", store.to_str().unwrap(), "--candidate", cand.to_str().unwrap(),
    ]);
    assert_eq!(data(&r)["reproduced"].as_bool(), Some(true), "{r}");

    // A regeneration that keeps the old name does not.
    std::fs::write(&cand, ADD).unwrap();
    let r = json_ok(dir.path(), env_root.path(), &[
        "op", "replay", &rename, "--store", store.to_str().unwrap(), "--candidate", cand.to_str().unwrap(),
    ]);
    assert_eq!(data(&r)["reproduced"].as_bool(), Some(false), "{r}");
}
