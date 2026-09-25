//! #837 piece D: `lex export-git --incremental` resumes an export from the
//! `Op:` trailer of the existing repo's HEAD, appending only the ops after it.
//!
//! The strong property: exporting the first k ops and then running
//! `--incremental` over the full store yields a repo IDENTICAL to one full
//! export of all the ops — same commits, same tree at every commit, same
//! messages, and (dates pinned) the same commit SHAs. Everything else here is
//! the safety net around it: never rewrite history, refuse anything that is not
//! a pure extension, verify the tree before appending, be idempotent.
//!
//! A store is "cut at k ops" by pointing its branch head back at the k-th op:
//! the exporter only walks from the head, so that is exactly a store built up to
//! k ops, and restoring the head afterwards is "the remaining ops arrived".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lex_store::{FileEntry, Manifest, Store, DEFAULT_BRANCH};
use lex_vcs::{Intent, IntentLog, ModelDescriptor, OpLog, OperationRecord, Origin, Person};
use tempfile::{tempdir, TempDir};

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── running the real binary ────────────────────────────────────────────────

/// `lex` with a hermetic git environment and both git dates pinned, so native
/// commits (which stamp the current time) are reproducible and SHAs comparable.
fn lex(args: &[&str]) -> Output {
    let home = tempdir().unwrap();
    Command::new(lex_bin())
        .env("HOME", home.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .args(args)
        .output()
        .unwrap()
}

fn export(store: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut a = vec!["--output", "json", "export-git", out.to_str().unwrap(), "--store", store.to_str().unwrap()];
    a.extend_from_slice(extra);
    lex(&a)
}

fn export_ok(store: &Path, out: &Path, extra: &[&str]) -> serde_json::Value {
    let res = export(store, out, extra);
    assert!(res.status.success(), "export {extra:?}: {}", String::from_utf8_lossy(&res.stderr));
    serde_json::from_slice::<serde_json::Value>(&res.stdout).unwrap()["data"].clone()
}

/// Where the failure text lands: stderr in text mode, the ACLI error envelope
/// on stdout under `--output json`. Either way, both are searched.
fn stderr(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// Everything observable about a repo's history: per commit its SHA, tree,
/// identities/dates and raw message.
fn history(dir: &Path) -> String {
    git(dir, &["log", "--reverse", "--format=%H%n%T%n%an|%ae|%aI|%cn|%ce|%cI%n%B%n--END--"])
}

fn count(dir: &Path) -> usize {
    git(dir, &["rev-list", "--count", "HEAD"]).trim().parse().unwrap()
}

/// The repo's full on-disk state a refused run must leave alone.
fn snapshot(dir: &Path) -> (String, String, Vec<u8>) {
    (
        git(dir, &["rev-parse", "HEAD"]),
        git(dir, &["status", "--porcelain", "--ignored"]),
        std::fs::read(dir.join(".git/config")).unwrap(),
    )
}

// ── cutting a store at k ops ───────────────────────────────────────────────

fn branch_file(store: &Path) -> PathBuf {
    store.join("branches").join(format!("{DEFAULT_BRANCH}.json"))
}

fn head_of(store: &Path) -> String {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(branch_file(store)).unwrap()).unwrap();
    v["head_op"].as_str().unwrap().to_string()
}

fn set_head(store: &Path, op: &str) {
    let p = branch_file(store);
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    v["head_op"] = op.into();
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
}

fn ops(store: &Path) -> Vec<OperationRecord> {
    OpLog::open(store).unwrap().walk_forward(&head_of(store), None).unwrap()
}

/// Export the first `k` ops, then bring the rest in and export `--incremental`.
/// Returns `(incremental repo, one-shot repo of all ops, incremental's JSON)`.
fn cut_and_resume(store: &Path, k: usize) -> (TempDir, TempDir, serde_json::Value) {
    let all = ops(store);
    let full_head = head_of(store);
    let reference = tempdir().unwrap();
    export_ok(store, reference.path(), &[]);

    let inc = tempdir().unwrap();
    set_head(store, &all[k - 1].op_id);
    export_ok(store, inc.path(), &[]);
    set_head(store, &full_head);
    let data = export_ok(store, inc.path(), &["--incremental"]);
    (inc, reference, data)
}

fn assert_identical(inc: &Path, reference: &Path, what: &str) {
    assert_eq!(count(inc), count(reference), "{what}: commit count");
    assert_eq!(history(inc), history(reference), "{what}: history (SHAs, trees, messages)");
    assert_eq!(
        git(inc, &["ls-files"]),
        git(reference, &["ls-files"]),
        "{what}: tracked files"
    );
    assert_eq!(git(inc, &["status", "--porcelain"]), "", "{what}: clean tree");
}

// ── scenario 1: native ops from `lex publish` (multi-file, local alias) ────

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn publish(work: &Path, pkg: &Path, store: &Path, intent: &[&str]) {
    let home = tempdir().unwrap();
    let mut args = vec!["publish", pkg.to_str().unwrap(), "--store", store.to_str().unwrap(), "--activate"];
    args.extend_from_slice(intent);
    let out = Command::new(lex_bin()).current_dir(work).env("HOME", home.path()).args(&args).output().unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
}

/// A multi-file package with a local aliased import, non-.lex files (SetFiles
/// ops), published three times under different pinned intents.
fn native_store() -> (TempDir, PathBuf) {
    let work = tempdir().unwrap();
    let pkg = work.path().join("pkg");
    let store = work.path().join("store");
    write(&pkg, "lex.toml", "[package]\nname = \"fxpkg\"\nversion = \"0.1.0\"\n");
    write(&pkg, "src/error.lex", "type Err = { code :: Int, msg :: Str }\n\nfn format(e :: Err) -> Str {\n  e.msg\n}\n");
    write(&pkg, "src/main.lex", "import \"./error\" as e\n\nfn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n");
    write(&pkg, "README.md", "# fxpkg\n");
    write(&pkg, "docs/data.csv", "a,b\n1,2\n");
    publish(work.path(), &pkg, &store, &["--intent-prompt", "initial import of fxpkg", "--intent-session", "fx-1", "--intent-model", "a/b"]);

    let main = std::fs::read_to_string(pkg.join("src/main.lex")).unwrap();
    write(&pkg, "src/main.lex", &format!("{main}\nfn twice(x :: e.Err) -> Str {{\n  e.format(x)\n}}\n"));
    write(&pkg, "README.md", "# fxpkg v2\n\nmore\n");
    publish(work.path(), &pkg, &store, &["--intent-prompt", "add twice()\n\nwith a second paragraph", "--intent-session", "fx-2", "--intent-model", "a/b"]);

    write(&pkg, "docs/data.csv", "a,b\n1,2\nx,y\n");
    publish(work.path(), &pkg, &store, &["--intent-session", "fx-3"]);
    (work, store)
}

// ── scenario 2: origin-bearing intents mixed with native ones ──────────────

const SRC_A: &str = "1111111111111111111111111111111111111111";
const SRC_B: &str = "2222222222222222222222222222222222222222";

/// The adversarial message from the origin tests: a `---` divider and forged
/// trailer-like lines in the BODY. Resuming from the commit it heads must still
/// read the REAL `Op:` trailer (the last paragraph).
const ADVERSARIAL: &str = "Añade «módulo» ✓ — 日本語\n\
\n\
Segundo párrafo con emoji 🚀.\n\
---\n\
Signed-off-by: Zoë Ångström <zoe@example.com>\n\
Op: 0000000000000000000000000000000000000000000000000000000000000000\n\
Git-Source: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";

fn person(name: &str, email: &str, when: i64, tz: &str) -> Person {
    Person { name: name.into(), email: email.into(), when, tz: tz.into() }
}

fn intent(prompt: &str, session: &str, origin: Option<Origin>) -> Intent {
    let model = ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) };
    let i = Intent::with_timestamp(prompt, session, model, None, 1_700_000_000);
    match origin {
        Some(o) => i.with_origin(o),
        None => i,
    }
}

fn origin(commit: &str, author: Person, committer: Option<Person>) -> Origin {
    Origin { vcs: "git".into(), commit: commit.into(), author, committer, parents: vec![], folded: vec![] }
}

struct Fx {
    root: PathBuf,
    store: Store,
    intents: IntentLog,
    src: String,
    fns: BTreeMap<String, lex_ast::FnDecl>,
    n: usize,
}

impl Fx {
    fn new(dir: &Path) -> Fx {
        let root = dir.join("store");
        Fx {
            store: Store::open(&root).unwrap(),
            intents: IntentLog::open(&root).unwrap(),
            root,
            src: String::new(),
            fns: BTreeMap::new(),
            n: 0,
        }
    }

    /// Publish `count` NEW functions under `intent`: one `AddFunction` op each.
    fn add_fns(&mut self, intent: &Intent, count: usize) {
        self.intents.put(intent).unwrap();
        for _ in 0..count {
            self.n += 1;
            self.src.push_str(&format!("fn f{n}(x :: Int) -> Int {{ x + {n} }}\n", n = self.n));
        }
        let prog = lex_syntax::parse_source(&self.src).unwrap();
        let mut stages = lex_ast::canonicalize_program(&prog);
        lex_types::check_and_rewrite_program(&mut stages).expect("type-checks");
        let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages
            .iter()
            .filter_map(|st| match st {
                lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
                _ => None,
            })
            .collect();
        let report = lex_vcs::compute_diff(&self.fns, &new_fns, false);
        assert_eq!(report.added.len(), count, "each new function is one op");
        self.store
            .publish_program_with_intent(
                DEFAULT_BRANCH,
                &stages,
                &report,
                &Default::default(),
                true,
                None,
                Some(intent.intent_id.clone()),
                &Default::default(),
            )
            .expect("publish");
        self.fns = new_fns;
    }

    /// Append one `SetFiles` op (a full snapshot) under `intent`.
    fn set_files(&self, intent: &Intent, files: &[(&str, &[u8], &str)]) {
        self.intents.put(intent).unwrap();
        let mut m = Manifest::new();
        for (path, bytes, mode) in files {
            let blob = self.store.put_blob_bytes(bytes).unwrap();
            m.entries.insert(
                (*path).to_string(),
                FileEntry { blob, mode: (*mode).to_string(), size: bytes.len() as u64 },
            );
        }
        let id = self.store.put_manifest(&m).unwrap();
        self.store.apply_set_files(DEFAULT_BRANCH, &id, Some(&intent.intent_id)).unwrap();
    }
}

/// A(5 fns + SetFiles) · B(2 fns) · A again(1 fn) · native N(2 fns, one intent).
/// 11 ops; the `.gitignore`d `build/out.log` in the SetFiles is a force-add case.
/// Commits: [A: ops 1-6] [B: 7-8] [A: 9] [N: 10] [N: 11].
fn origin_store() -> (TempDir, Fx, Intent) {
    let work = tempdir().unwrap();
    let mut fx = Fx::new(work.path());
    let ia = intent(
        ADVERSARIAL,
        "git-import:root",
        Some(origin(
            SRC_A,
            person("Zoë Ångström", "zoe@example.com", 1_700_000_000, "+0530"),
            Some(person("Ann Committer", "ann@example.org", 1_700_000_100, "-0500")),
        )),
    );
    let ib = intent(
        "second commit\n",
        "git-import:root",
        Some(origin(SRC_B, person("Bo Négatif", "bo@example.net", 1_600_000_000, "-0700"), None)),
    );
    let native = intent("native work", "cli-1", None);
    fx.add_fns(&ia, 5);
    fx.set_files(
        &ia,
        &[
            ("README.md", b"hello\n", "100644"),
            ("bin/run.sh", b"#!/bin/sh\n", "100755"),
            (".gitignore", b"*.log\n", "100644"),
            ("build/out.log", b"force-added\n", "100644"),
        ],
    );
    fx.add_fns(&ib, 2);
    fx.add_fns(&ia, 1);
    fx.add_fns(&native, 1);
    fx.add_fns(&native, 1);
    (work, fx, ia)
}

// ── 1. the strong property ─────────────────────────────────────────────────

#[test]
fn native_store_resumed_at_every_k_equals_a_full_export() {
    let (_w, store) = native_store();
    let n = ops(&store).len();
    assert!(n >= 8, "the fixture has a real number of ops, got {n}");
    // First op, every middle, last-1, and all.
    for k in 1..=n {
        let (inc, reference, data) = cut_and_resume(&store, k);
        assert_identical(inc.path(), reference.path(), &format!("native k={k}/{n}"));
        assert_eq!(data["mode"], "incremental");
        assert_eq!(data["exported_ops"], n - k, "k={k}");
        assert_eq!(data["new_commits"], n - k, "one commit per native op, k={k}");
        assert_eq!(data["resumed_from"], ops(&store)[k - 1].op_id, "k={k}");
    }
}

#[test]
fn origin_store_resumed_at_every_group_boundary_equals_a_full_export() {
    let (_w, fx, _ia) = origin_store();
    assert_eq!(ops(&fx.root).len(), 11);
    // Group boundaries: after A(6), B(8), A again(9), the first native op(10),
    // and all(11). (A cut INSIDE a group is the next test.)
    for (k, new_commits) in [(6usize, 4usize), (8, 3), (9, 2), (10, 1), (11, 0)] {
        let (inc, reference, data) = cut_and_resume(&fx.root, k);
        assert_identical(inc.path(), reference.path(), &format!("origin k={k}"));
        assert_eq!(data["new_commits"], new_commits, "k={k}");
        assert_eq!(data["exported_ops"], 11 - k, "k={k}");
        // The force-added, gitignored file is in every tree (verify used `-f` too).
        assert_eq!(
            git(inc.path(), &["show", "HEAD:build/out.log"]),
            "force-added\n",
            "k={k}"
        );
    }
}

/// A cut inside an origin group: the commit already in the repo holds only the
/// ops up to the marker. The rest of the group is a NEW commit (never an amend).
#[test]
fn resuming_inside_an_origin_group_appends_a_new_commit_and_never_amends() {
    let (_w, fx, ia) = origin_store();
    let all = ops(&fx.root);
    let (inc, reference, data) = cut_and_resume(&fx.root, 3);

    // The one-shot export folds ops 1-6 into ONE commit; the resumed repo has
    // ops 1-3 (already there) and 4-6 as a second commit.
    assert_eq!(count(inc.path()), count(reference.path()) + 1);
    assert_eq!(data["new_commits"], 5, "ops 4-6 (one commit) + B + A again + 2 native");
    let msgs = git(inc.path(), &["log", "--reverse", "-z", "--format=%B"]);
    let msgs: Vec<&str> = msgs.split('\0').filter(|m| !m.trim().is_empty()).collect();
    assert!(msgs[0].contains("\nOps: 3\n") && msgs[0].contains(&format!("\nOp: {}\n", all[2].op_id)), "{}", msgs[0]);
    assert!(msgs[1].contains("\nOps: 3\n") && msgs[1].contains(&format!("\nOp: {}\n", all[5].op_id)), "{}", msgs[1]);
    for m in &msgs[..2] {
        assert!(m.contains(&format!("\nIntent: {}\n", ia.intent_id)), "{m}");
        assert!(m.contains(&format!("\nGit-Source: {SRC_A}\n")), "{m}");
    }
    // From the group's completion on it is the one-shot history again, tree
    // for tree: the extra commit is the partial one at the front.
    let trees = |d: &Path| -> Vec<String> {
        git(d, &["log", "--reverse", "--format=%T"]).lines().map(String::from).collect()
    };
    assert_eq!(trees(inc.path())[1..], trees(reference.path())[..]);
}

// ── 2. output, fresh directories, idempotence ──────────────────────────────

#[test]
fn incremental_on_a_fresh_dir_is_a_full_export() {
    let (_w, store) = native_store();
    let reference = tempdir().unwrap();
    let plain = export_ok(&store, reference.path(), &[]);
    // The non-incremental output keeps exactly its old shape.
    let mut keys: Vec<&String> = plain.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["branch", "commits", "out_dir"]);

    let fresh = tempdir().unwrap();
    let data = export_ok(&store, fresh.path(), &["--incremental"]);
    assert_identical(fresh.path(), reference.path(), "fresh --incremental");
    assert_eq!(data["mode"], "incremental");
    assert!(data["resumed_from"].is_null());
    assert_eq!(data["exported_ops"], data["new_commits"]);
    assert_eq!(data["commits"], plain["commits"]);

    // An empty, already-`git init`ed directory is fresh too.
    let empty_repo = tempdir().unwrap();
    git(empty_repo.path(), &["init", "-q"]);
    export_ok(&store, empty_repo.path(), &["--incremental"]);
    assert_identical(empty_repo.path(), reference.path(), "empty repo --incremental");
}

#[test]
fn rerunning_with_no_new_ops_creates_zero_commits_and_one_new_op_one_commit() {
    let (_w, mut fx, _ia) = origin_store();
    let out = tempdir().unwrap();
    export_ok(&fx.root, out.path(), &[]);
    let before = snapshot(out.path());
    let history_before = history(out.path());

    for _ in 0..2 {
        let res = lex(&["export-git", out.path().to_str().unwrap(), "--store", fx.root.to_str().unwrap(), "--incremental"]);
        assert!(res.status.success(), "{}", stderr(&res));
        let text = String::from_utf8_lossy(&res.stdout);
        assert!(text.contains("nothing to export"), "{text}");
        assert_eq!(snapshot(out.path()), before, "a no-op run touches nothing");
    }
    let data = export_ok(&fx.root, out.path(), &["--incremental"]);
    assert_eq!(data["new_commits"], 0);
    assert_eq!(data["exported_ops"], 0);
    assert_eq!(history(out.path()), history_before);

    // One new op => exactly one new commit, and the result is the one-shot export.
    fx.add_fns(&intent("one more", "cli-2", None), 1);
    let data = export_ok(&fx.root, out.path(), &["--incremental"]);
    assert_eq!(data["new_commits"], 1);
    assert_eq!(data["exported_ops"], 1);
    assert_eq!(count(out.path()), 6);
    let reference = tempdir().unwrap();
    export_ok(&fx.root, reference.path(), &[]);
    assert_identical(out.path(), reference.path(), "one new op");
}

#[test]
fn no_verify_requires_incremental() {
    let (_w, store) = native_store();
    let out = tempdir().unwrap();
    let res = export(&store, out.path(), &["--no-verify"]);
    assert!(!res.status.success());
    assert!(stderr(&res).contains("--no-verify only applies to --incremental"), "{}", stderr(&res));
}

// ── 3. refusals: nothing is touched ────────────────────────────────────────

fn assert_refused(res: &Output, needle: &str) {
    assert!(!res.status.success(), "must exit non-zero, got: {}", String::from_utf8_lossy(&res.stdout));
    assert!(stderr(res).contains(needle), "stderr should mention `{needle}`: {}", stderr(res));
}

/// An export of the first `k` ops in `out`, and the store restored to all ops.
fn partial_export(store: &Path, out: &Path, k: usize) {
    let all = ops(store);
    let full = head_of(store);
    set_head(store, &all[k - 1].op_id);
    export_ok(store, out, &[]);
    set_head(store, &full);
}

#[test]
fn a_marker_off_the_branch_history_is_refused() {
    let (_w, store) = native_store();
    let n = ops(&store).len();

    // (a) a repo exported from a DIFFERENT store.
    let (_w2, other) = {
        let (w, fx, _) = origin_store();
        (w, fx.root)
    };
    let out = tempdir().unwrap();
    export_ok(&store, out.path(), &[]);
    let before = snapshot(out.path());
    let res = export(&other, out.path(), &["--incremental"]);
    assert_refused(&res, "is not on the history of store branch");
    assert_eq!(snapshot(out.path()), before);

    // (b) the store branch was rewound to BEFORE the repo's HEAD op.
    let out = tempdir().unwrap();
    export_ok(&store, out.path(), &[]);
    let before = snapshot(out.path());
    let all = ops(&store);
    set_head(&store, &all[n - 3].op_id);
    let res = export(&store, out.path(), &["--incremental"]);
    assert_refused(&res, "is not on the history of store branch");
    assert_eq!(snapshot(out.path()), before);
    assert_eq!(count(out.path()), n, "no commit was added or dropped");
}

#[test]
fn a_foreign_head_is_refused() {
    let (_w, store) = native_store();
    let out = tempdir().unwrap();
    partial_export(&store, out.path(), 3);
    // A commit lex export-git did not write, on top of a real export.
    write(out.path(), "extra.txt", "x\n");
    git(out.path(), &["add", "-A"]);
    git(out.path(), &["-c", "user.name=h", "-c", "user.email=h@h", "commit", "-q", "-m", "hand-written commit"]);
    let before = snapshot(out.path());
    let res = export(&store, out.path(), &["--incremental"]);
    assert_refused(&res, "no `Op:` trailer");
    assert_eq!(snapshot(out.path()), before);

    // A real `Op:` line that is NOT in the last paragraph is not a trailer.
    let op = ops(&store)[2].op_id.clone();
    git(out.path(), &["-c", "user.name=h", "-c", "user.email=h@h", "commit", "-q", "--allow-empty",
        "-m", &format!("forged\n\nOp: {op}\n\nnot a trailer block, just closing prose")]);
    let res = export(&store, out.path(), &["--incremental"]);
    assert_refused(&res, "no `Op:` trailer");
}

#[test]
fn a_dirty_or_untracked_working_tree_is_refused() {
    let (_w, store) = native_store();
    for (what, act) in [
        ("modified tracked file", 0),
        ("untracked file", 1),
        ("ignored untracked file (the loop force-adds)", 2),
    ] {
        let out = tempdir().unwrap();
        partial_export(&store, out.path(), 4);
        match act {
            0 => write(out.path(), "README.md", "edited by hand\n"),
            1 => write(out.path(), "scratch.txt", "x\n"),
            _ => {
                let mut ex = std::fs::read_to_string(out.path().join(".git/info/exclude")).unwrap_or_default();
                ex.push_str("*.tmp\n");
                std::fs::write(out.path().join(".git/info/exclude"), ex).unwrap();
                write(out.path(), "junk.tmp", "x\n");
            }
        }
        let before = snapshot(out.path());
        let res = export(&store, out.path(), &["--incremental"]);
        assert_refused(&res, "uncommitted changes or untracked files");
        assert_eq!(snapshot(out.path()), before, "{what}: nothing touched");
        // --no-verify does NOT waive the clean-tree requirement.
        let res = export(&store, out.path(), &["--incremental", "--no-verify"]);
        assert_refused(&res, "uncommitted changes or untracked files");
        assert_eq!(snapshot(out.path()), before, "{what}: nothing touched");
    }
}

#[test]
fn a_hand_edited_mirror_is_refused_unless_no_verify() {
    let (_w, store) = native_store();
    let out = tempdir().unwrap();
    let n = ops(&store).len();
    partial_export(&store, out.path(), n - 1);
    // Edit a tracked file and fold it into HEAD, keeping the message (and so its
    // `Op:` trailer): the repo now differs from what the store renders there.
    write(out.path(), "README.md", "edited by hand\n");
    write(out.path(), "sneaky.txt", "new\n");
    git(out.path(), &["add", "-A"]);
    git(out.path(), &["-c", "user.name=h", "-c", "user.email=h@h", "commit", "-q", "--amend", "--no-edit"]);
    let before = snapshot(out.path());

    let res = export(&store, out.path(), &["--incremental"]);
    assert_refused(&res, "edited outside `lex export-git`");
    let err = stderr(&res);
    assert!(err.contains("differs: README.md"), "{err}");
    assert!(err.contains("not in the store's rendering: sneaky.txt"), "{err}");
    assert_eq!(snapshot(out.path()), before, "refusal touches nothing");

    let data = export_ok(&store, out.path(), &["--incremental", "--no-verify"]);
    assert_eq!(data["new_commits"], 1);
    assert_eq!(count(out.path()), n);
}

#[test]
fn a_marker_from_another_intent_is_refused() {
    // HEAD's `Intent:` trailer must belong to the op its `Op:` names.
    let (_w, store) = native_store();
    let out = tempdir().unwrap();
    partial_export(&store, out.path(), 3);
    let op = ops(&store)[2].op_id.clone();
    git(out.path(), &["-c", "user.name=h", "-c", "user.email=h@h", "commit", "-q", "--allow-empty",
        "-m", &format!("mismatched\n\nOp: {op}\nIntent: {}", "f".repeat(64))]);
    let res = export(&store, out.path(), &["--incremental"]);
    assert_refused(&res, "`Intent:` trailer");
}
