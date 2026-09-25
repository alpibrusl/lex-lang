//! `lex export-git` must reproduce a manifest MODE change, not only a content
//! change: a commit that flips `bin/run` from `100755` to `100644` (or back)
//! with the SAME blob has to show up in the exported git history as a mode
//! change, at every commit, in the full export, in an `--incremental` resume,
//! for origin-bearing grouped commits, and whatever the host's git config says
//! about `core.fileMode`.
//!
//! Which layer lost the bit (found by writing the failing test first): the
//! manifest diff was fine (`apply_manifest_diff` compares whole `FileEntry`s,
//! mode included, so a mode-only change did reach `write_manifest_entry`), and
//! git was fine; `write_manifest_entry` only ever SET the exec bit, and
//! `fs::write` onto an existing file keeps its permissions, so the on-disk file
//! stayed executable and `git add -A` re-staged `100755`. The tests below assert
//! the mode per commit from `git ls-tree` / `git diff-tree`, not just
//! "resume equals one-shot" (both sides shared the bug).
//!
//! Mutation checks (each must turn the named tests red, see the PR):
//! * `write_manifest_entry` never clears the bit (the original bug): every
//!   `*_to_plain*` / chain / resume / group / import test.
//! * `pin_file_mode` a no-op: `a_mirror_with_core_filemode_false_...`.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lex_store::{FileEntry, Manifest, Store, DEFAULT_BRANCH};
use lex_vcs::{Intent, IntentLog, ModelDescriptor, OpLog, OperationRecord, Origin, Person};
use tempfile::{tempdir, TempDir};

const X: &str = "100755";
const P: &str = "100644";

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── running the real binary ────────────────────────────────────────────────

/// `lex` with hermetic git config (or, for the hostile tests, a global config
/// file the caller wrote) and both git dates pinned.
fn lex_with(global_cfg: Option<&Path>, args: &[&str]) -> Output {
    let home = tempdir().unwrap();
    let mut c = Command::new(lex_bin());
    c.env("HOME", home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL");
    match global_cfg {
        Some(p) => c.env("GIT_CONFIG_GLOBAL", p),
        None => c.env("GIT_CONFIG_GLOBAL", "/dev/null"),
    };
    c.args(args).output().unwrap()
}

fn export_with(global_cfg: Option<&Path>, store: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut a = vec!["--output", "json", "export-git", out.to_str().unwrap(), "--store", store.to_str().unwrap()];
    a.extend_from_slice(extra);
    lex_with(global_cfg, &a)
}

fn export_ok(store: &Path, out: &Path, extra: &[&str]) {
    let res = export_with(None, store, out, extra);
    assert!(res.status.success(), "export {extra:?}: {}", String::from_utf8_lossy(&res.stderr));
}

fn combined(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stderr), String::from_utf8_lossy(&o.stdout))
}

/// Hermetic `git -C dir ...`, pinned identity so `commit`/`--amend` never read
/// the developer's config.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn history(dir: &Path) -> String {
    git(dir, &["log", "--reverse", "--format=%H%n%T%n%an|%ae|%aI|%cn|%ce|%cI%n%B%n--END--"])
}

fn commits(dir: &Path) -> Vec<String> {
    git(dir, &["rev-list", "--reverse", "HEAD"]).lines().map(str::to_string).collect()
}

/// `path -> (mode, blob)` of the tracked files a commit holds.
fn tree_at(dir: &Path, rev: &str) -> BTreeMap<String, (String, String)> {
    git(dir, &["ls-tree", "-r", rev])
        .lines()
        .map(|l| {
            let (meta, path) = l.split_once('\t').unwrap();
            let mut m = meta.split(' ');
            let mode = m.next().unwrap().to_string();
            let _ = m.next();
            (path.to_string(), (mode, m.next().unwrap().to_string()))
        })
        .collect()
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
}

// ── cutting a store at k ops (see export_git_incremental_837.rs) ───────────

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

/// Export the first `k` ops, bring the rest in, `--incremental`. Returns
/// `(incremental repo, one-shot repo of all ops)`.
fn cut_and_resume(store: &Path, k: usize) -> (TempDir, TempDir) {
    let all = ops(store);
    let full_head = head_of(store);
    let reference = tempdir().unwrap();
    export_ok(store, reference.path(), &[]);
    let inc = tempdir().unwrap();
    set_head(store, &all[k - 1].op_id);
    export_ok(store, inc.path(), &[]);
    set_head(store, &full_head);
    export_ok(store, inc.path(), &["--incremental"]);
    (inc, reference)
}

// ── fixtures ────────────────────────────────────────────────────────────────

fn person(name: &str, when: i64) -> Person {
    Person { name: name.into(), email: format!("{name}@example.org"), when, tz: "+0100".into() }
}

fn intent(prompt: &str, session: &str, origin_commit: Option<&str>) -> Intent {
    let model = ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) };
    let i = Intent::with_timestamp(prompt, session, model, None, 1_700_000_000);
    match origin_commit {
        Some(c) => i.with_origin(Origin {
            vcs: "git".into(),
            commit: c.into(),
            author: person("ada", 1_700_000_000),
            committer: None,
            parents: vec![],
            folded: vec![],
        }),
        None => i,
    }
}

/// An origin-bearing intent with a distinct fake source commit `n`.
fn origin_intent(n: usize) -> Intent {
    let commit = format!("{n:0>40}");
    intent(&format!("imported commit {n}\n"), "git-import:root", Some(&commit))
}

fn native_intent(n: usize) -> Intent {
    intent(&format!("native change {n}"), "cli-1", None)
}

struct Fx {
    store: Store,
    intents: IntentLog,
    src: String,
    fns: BTreeMap<String, lex_ast::FnDecl>,
    n: usize,
}

impl Fx {
    fn new(root: &Path) -> Fx {
        Fx {
            store: Store::open(root).unwrap(),
            intents: IntentLog::open(root).unwrap(),
            src: String::new(),
            fns: BTreeMap::new(),
            n: 0,
        }
    }

    fn add_fn(&mut self, intent: &Intent) {
        self.intents.put(intent).unwrap();
        self.n += 1;
        self.src.push_str(&format!("fn f{n}(x :: Int) -> Int {{ x + {n} }}\n", n = self.n));
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
        assert_eq!(report.added.len(), 1);
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

    /// One `SetFiles` op holding exactly `files` (path, bytes, mode).
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

/// The two tracked scripts (one top-level, one nested under `src/`, which the
/// exporter wipes and re-materializes on every commit) at each step of the
/// chain, as `(bin/run bytes, src/tool.sh bytes, mode)`:
///
/// | step | change                              |
/// |------|-------------------------------------|
/// | 0    | created executable                  |
/// | 1    | mode only, 100755 -> 100644         |
/// | 2    | mode only, 100644 -> 100755         |
/// | 3    | content AND mode, 100755 -> 100644  |
/// | 4    | content AND mode, 100644 -> 100755  |
const STEPS: [(&[u8], &[u8], &str); 5] = [
    (b"#!/bin/sh\necho 1\n", b"tool 1\n", X),
    (b"#!/bin/sh\necho 1\n", b"tool 1\n", P),
    (b"#!/bin/sh\necho 1\n", b"tool 1\n", X),
    (b"#!/bin/sh\necho 2\n", b"tool 2\n", P),
    (b"#!/bin/sh\necho 3\n", b"tool 3\n", X),
];

fn step_files(i: usize) -> Vec<(&'static str, &'static [u8], &'static str)> {
    let (run, tool, mode) = STEPS[i];
    vec![("README.md", b"# readme\n", P), ("bin/run", run, mode), ("src/tool.sh", tool, mode)]
}

/// Native store: one `SetFiles` op (hence one commit) per step.
fn native_chain(dir: &Path) -> PathBuf {
    let root = dir.join("store");
    let fx = Fx::new(&root);
    for i in 0..STEPS.len() {
        fx.set_files(&native_intent(i), &step_files(i));
    }
    root
}

/// Origin-bearing store: five commits, the first two GROUPED with a function
/// declaration so the mode change lands inside a multi-op commit.
///
///   commit 0: AddFunction + SetFiles(step 0)   (one intent, 2 ops)
///   commit 1: AddFunction + SetFiles(step 1)   (one intent, 2 ops) mode-only flip
///   commits 2..4: SetFiles(step 2..4)          (one op each)
fn origin_chain(dir: &Path) -> PathBuf {
    let root = dir.join("store");
    let mut fx = Fx::new(&root);
    for i in 0..2 {
        let it = origin_intent(i + 1);
        fx.add_fn(&it);
        fx.set_files(&it, &step_files(i));
    }
    for i in 2..STEPS.len() {
        fx.set_files(&origin_intent(i + 1), &step_files(i));
    }
    root
}

/// The exported history holds `bin/run` and `src/tool.sh` with the mode (and
/// blob) the manifest had at each commit, the working tree agrees, and every
/// commit after the first shows the transition as a `git diff-tree` change.
fn assert_chain_exported(repo: &Path, what: &str) {
    let revs = commits(repo);
    assert_eq!(revs.len(), STEPS.len(), "{what}: one commit per step");
    for (i, rev) in revs.iter().enumerate() {
        let t = tree_at(repo, rev);
        let (run, tool, mode) = STEPS[i];
        for (path, bytes) in [("bin/run", run), ("src/tool.sh", tool)] {
            let (got_mode, got_blob) = &t[path];
            assert_eq!(got_mode, mode, "{what}: mode of {path} in commit {i}");
            let want_blob = {
                let mut child = Command::new("git")
                    .args(["hash-object", "--stdin"])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                use std::io::Write;
                child.stdin.take().unwrap().write_all(bytes).unwrap();
                String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap().trim().to_string()
            };
            assert_eq!(got_blob, &want_blob, "{what}: blob of {path} in commit {i}");
        }
    }
    // `git diff-tree` sees each transition as a mode change (same blob) or a
    // mode+content change, never as "nothing".
    for i in 1..revs.len() {
        let raw = git(repo, &["diff-tree", "-r", "--no-commit-id", &revs[i - 1], &revs[i]]);
        let (prev, cur) = (STEPS[i - 1].2, STEPS[i].2);
        for path in ["bin/run", "src/tool.sh"] {
            let line = raw
                .lines()
                .find(|l| l.ends_with(&format!("\t{path}")))
                .unwrap_or_else(|| panic!("{what}: commit {i} must change {path}:\n{raw}"));
            assert!(line.starts_with(&format!(":{prev} {cur} ")), "{what}: commit {i} {path}: {line}");
            let f: Vec<&str> = line.split_whitespace().collect(); // :old new oldblob newblob M path
            let content_changed = STEPS[i - 1].0 != STEPS[i].0;
            assert_eq!(f[2] != f[3], content_changed, "{what}: commit {i} {path} blob change: {line}");
        }
    }
    // `git log --raw` agrees for the mode-only commits (the reported shape).
    let raw_log = git(repo, &["log", "--raw", "--no-abbrev", "--format=%s"]);
    assert!(raw_log.contains(&format!(":{X} {P} ")), "{what}: a 755->644 change in the log:\n{raw_log}");
    assert!(raw_log.contains(&format!(":{P} {X} ")), "{what}: a 644->755 change in the log:\n{raw_log}");
    // The working tree at HEAD carries the final mode, and git sees it clean.
    assert!(is_exec(&repo.join("bin/run")), "{what}: final on-disk bin/run is executable");
    assert!(is_exec(&repo.join("src/tool.sh")), "{what}: final on-disk src/tool.sh is executable");
    assert_eq!(git(repo, &["status", "--porcelain"]), "", "{what}: clean tree");
}

// ── 1. the reported bug and its mirror image, one transition per test ──────

/// A store with just two commits: `bin/run` and its `before` -> `after` mode
/// (same blob unless `new_bytes`).
fn two_step_store(dir: &Path, before: &str, after: &str, new_bytes: Option<&[u8]>) -> PathBuf {
    let root = dir.join("store");
    let fx = Fx::new(&root);
    let old: &[u8] = b"#!/bin/sh\necho hi\n";
    fx.set_files(&native_intent(0), &[("bin/run", old, before)]);
    fx.set_files(&native_intent(1), &[("bin/run", new_bytes.unwrap_or(old), after)]);
    root
}

fn check_two_step(before: &str, after: &str, new_bytes: Option<&[u8]>) {
    let w = tempdir().unwrap();
    let store = two_step_store(w.path(), before, after, new_bytes);
    let out = tempdir().unwrap();
    export_ok(&store, out.path(), &[]);
    let revs = commits(out.path());
    assert_eq!(revs.len(), 2);
    assert_eq!(tree_at(out.path(), &revs[0])["bin/run"].0, before, "commit 1 mode");
    assert_eq!(tree_at(out.path(), &revs[1])["bin/run"].0, after, "commit 2 mode");
    let raw = git(out.path(), &["diff-tree", "-r", "--no-commit-id", &revs[0], &revs[1]]);
    assert!(raw.starts_with(&format!(":{before} {after} ")), "a mode change, not nothing:\n{raw}");
    assert_eq!(is_exec(&out.path().join("bin/run")), after == X, "on-disk mode after commit 2");
    assert_eq!(git(out.path(), &["status", "--porcelain"]), "");
}

#[test]
fn mode_only_change_exec_to_plain_is_exported() {
    check_two_step(X, P, None);
}

#[test]
fn mode_only_change_plain_to_exec_is_exported() {
    check_two_step(P, X, None);
}

#[test]
fn mode_and_content_change_exec_to_plain_is_exported() {
    check_two_step(X, P, Some(b"#!/bin/sh\necho changed\n"));
}

#[test]
fn mode_and_content_change_plain_to_exec_is_exported() {
    check_two_step(P, X, Some(b"#!/bin/sh\necho changed\n"));
}

// ── 2. the whole chain, native and origin-bearing/grouped ──────────────────

#[test]
fn native_chain_reproduces_every_mode_transition() {
    let w = tempdir().unwrap();
    let store = native_chain(w.path());
    let out = tempdir().unwrap();
    export_ok(&store, out.path(), &[]);
    assert_chain_exported(out.path(), "native");
}

#[test]
fn origin_bearing_grouped_chain_reproduces_every_mode_transition() {
    let w = tempdir().unwrap();
    let store = origin_chain(w.path());
    assert_eq!(ops(&store).len(), 7, "2 grouped commits of 2 ops + 3 single-op commits");
    let out = tempdir().unwrap();
    export_ok(&store, out.path(), &[]);
    assert_chain_exported(out.path(), "origin");
    // The grouped commit that flips the mode is ONE commit carrying its origin.
    let body = git(out.path(), &["log", "-2", "--format=%B", "--skip=3"]);
    assert!(body.contains("Ops: 2"), "{body}");
}

// ── 3. --incremental ────────────────────────────────────────────────────────

/// First-k-then-resume equals one-shot AND both hold the right mode per commit,
/// for a resume point on either side of every mode transition.
fn resume_at_every_group_end(store: &Path, group_ends: &[usize], what: &str) {
    for &k in group_ends {
        let (inc, reference) = cut_and_resume(store, k);
        assert_eq!(history(inc.path()), history(reference.path()), "{what} k={k}: history");
        assert_eq!(git(inc.path(), &["ls-files", "-s"]), git(reference.path(), &["ls-files", "-s"]), "{what} k={k}: index modes");
        assert_chain_exported(inc.path(), &format!("{what} resumed at op {k}"));
    }
}

#[test]
fn native_chain_resumed_across_a_mode_change_equals_one_shot() {
    let w = tempdir().unwrap();
    let store = native_chain(w.path());
    resume_at_every_group_end(&store, &[1, 2, 3, 4, 5], "native");
}

#[test]
fn origin_chain_resumed_across_a_mode_change_equals_one_shot() {
    let w = tempdir().unwrap();
    let store = origin_chain(w.path());
    // Ops: [1,2]=commit 0, [3,4]=commit 1, 5, 6, 7.
    resume_at_every_group_end(&store, &[2, 4, 5, 6, 7], "origin");
}

/// The verification step compares (path, MODE, blob): a mirror whose HEAD tree
/// differs from the store's rendering only by an exec bit is refused, not
/// silently appended to.
#[test]
fn incremental_verification_sees_a_mode_only_difference() {
    let w = tempdir().unwrap();
    let store = native_chain(w.path());
    let all = ops(&store);
    let full_head = head_of(&store);
    let out = tempdir().unwrap();
    set_head(&store, &all[0].op_id);
    export_ok(&store, out.path(), &[]);
    // Someone drops the exec bit in the mirror and amends it into HEAD (keeping
    // its trailers, so it still looks like our commit).
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out.path().join("bin/run"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    git(out.path(), &["commit", "-q", "-a", "--amend", "--no-edit"]);
    assert_eq!(tree_at(out.path(), "HEAD")["bin/run"].0, P, "the tampering is a pure mode change");
    set_head(&store, &full_head);
    let res = export_with(None, &store, out.path(), &["--incremental"]);
    assert!(!res.status.success(), "must be refused");
    let text = combined(&res);
    assert!(text.contains("differs: bin/run"), "names the mode-differing path: {text}");
    // --no-verify appends anyway, and the appended commits carry the right modes.
    let res = export_with(None, &store, out.path(), &["--incremental", "--no-verify"]);
    assert!(res.status.success(), "{}", combined(&res));
    let revs = commits(out.path());
    assert_eq!(tree_at(out.path(), &revs[1])["bin/run"].0, P);
    assert_eq!(tree_at(out.path(), &revs[2])["bin/run"].0, X);
}

// ── 4. hostile git config ──────────────────────────────────────────────────

/// A global config with `core.fileMode=false` (what a checkout on some
/// filesystems ends up with) must not hide the mode change from the export.
#[test]
fn a_hostile_global_core_filemode_false_does_not_hide_mode_changes() {
    let w = tempdir().unwrap();
    let cfg = w.path().join("hostile.gitconfig");
    std::fs::write(&cfg, "[core]\n\tfileMode = false\n[user]\n\tname = hostile\n\temail = h@h\n").unwrap();
    let store = native_chain(w.path());
    let out = tempdir().unwrap();
    let res = export_with(Some(&cfg), &store, out.path(), &[]);
    assert!(res.status.success(), "{}", combined(&res));
    assert_chain_exported(out.path(), "hostile global config");
    assert_eq!(git(out.path(), &["config", "--local", "core.fileMode"]).trim(), "true");
}

/// A PRE-EXISTING mirror whose own config says `core.fileMode=false` (created
/// on a host that had it off) is appended to by `--incremental`: the mode change
/// must still land, so the exporter pins the repo's `core.fileMode`.
#[test]
fn a_mirror_with_core_filemode_false_still_records_mode_changes_on_resume() {
    let w = tempdir().unwrap();
    let store = native_chain(w.path());
    let all = ops(&store);
    let full_head = head_of(&store);
    let inc = tempdir().unwrap();
    set_head(&store, &all[0].op_id);
    export_ok(&store, inc.path(), &[]);
    git(inc.path(), &["config", "core.fileMode", "false"]);
    set_head(&store, &full_head);
    // Hostile global config too.
    let cfg = w.path().join("hostile.gitconfig");
    std::fs::write(&cfg, "[core]\n\tfileMode = false\n").unwrap();
    let res = export_with(Some(&cfg), &store, inc.path(), &["--incremental"]);
    assert!(res.status.success(), "{}", combined(&res));
    assert_chain_exported(inc.path(), "mirror with fileMode=false");
}

// ── 5. import-git of a repo that drops an exec bit ─────────────────────────

fn write(dir: &Path, name: &str, contents: &[u8]) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, contents).unwrap();
}

fn commit_env(dir: &Path, msg: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["commit", "-q", "--allow-empty", "--cleanup=verbatim", "-m", msg])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.org")
        .env("GIT_AUTHOR_DATE", "2024-03-05T10:20:30+0530")
        .env("GIT_COMMITTER_NAME", "Carl")
        .env("GIT_COMMITTER_EMAIL", "carl@example.org")
        .env("GIT_COMMITTER_DATE", "2024-03-06T23:59:01-0700")
        .output()
        .unwrap();
    assert!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn import(repo: &Path, branch: &str, store: &Path) {
    let res = lex_with(
        None,
        &["op", "import-git", repo.to_str().unwrap(), "--branch", branch, "--store-branch", "main", "--head-only", "--store", store.to_str().unwrap()],
    );
    assert!(res.status.success(), "import: {}", combined(&res));
}

/// A non-Lex source repo whose second commit ONLY drops `bin/run`'s exec bit.
/// `import-git --head-only` reads the mode of the tip's `ls-tree` entry; the
/// export must reproduce the tip's tree exactly (mode included), and a store
/// that holds the exec commit first and the mode drop after it must export
/// both source trees, tree id for tree id.
#[test]
fn import_of_a_repo_that_drops_an_exec_bit_round_trips_the_mode_change() {
    let w = tempdir().unwrap();
    let repo = w.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, "README.md", b"# docs\n");
    write(&repo, "bin/run", b"#!/bin/sh\necho hi\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join("bin/run"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&repo, &["add", "-A"]);
    let c1 = commit_env(&repo, "add run script");
    git(&repo, &["update-index", "--chmod=-x", "bin/run"]);
    let c2 = commit_env(&repo, "drop the exec bit");
    assert_eq!(tree_at(&repo, &c1)["bin/run"].0, X);
    assert_eq!(tree_at(&repo, &c2)["bin/run"].0, P, "the source commit is a pure mode change");
    assert_eq!(tree_at(&repo, &c1)["bin/run"].1, tree_at(&repo, &c2)["bin/run"].1, "same blob");
    let tree = |rev: &str| git(&repo, &["rev-parse", &format!("{rev}^{{tree}}")]).trim().to_string();

    // (a) The tip alone: imported as 100644, exported as 100644.
    let store_tip = w.path().join("tip-store");
    import(&repo, "main", &store_tip);
    let out_tip = w.path().join("tip-out");
    export_ok(&store_tip, &out_tip, &[]);
    let revs = commits(&out_tip);
    assert_eq!(revs.len(), 1);
    assert_eq!(tree_at(&out_tip, &revs[0])["bin/run"].0, P);
    assert_eq!(git(&out_tip, &["rev-parse", "HEAD^{tree}"]).trim(), tree(&c2), "tip tree reproduced");
    assert!(!is_exec(&out_tip.join("bin/run")));

    // (b) The parent imported (100755), then the drop arrives as a later op on
    // the same branch (as the history importer will lay it down): the export
    // is two commits whose trees are the source's two trees.
    git(&repo, &["branch", "at-c1", &c1]);
    let store_hist = w.path().join("store");
    import(&repo, "at-c1", &store_hist);
    let fx = Fx::new(&store_hist);
    let it = origin_intent(2);
    fx.set_files(&it, &[("README.md", b"# docs\n", P), ("bin/run", b"#!/bin/sh\necho hi\n", P)]);
    let out = w.path().join("out");
    export_ok(&store_hist, &out, &[]);
    let revs = commits(&out);
    assert_eq!(revs.len(), 2);
    assert_eq!(git(&out, &["rev-parse", &format!("{}^{{tree}}", revs[0])]).trim(), tree(&c1), "commit 1 tree");
    assert_eq!(git(&out, &["rev-parse", &format!("{}^{{tree}}", revs[1])]).trim(), tree(&c2), "commit 2 tree");
    let raw = git(&out, &["diff-tree", "-r", "--no-commit-id", &revs[0], &revs[1]]);
    assert!(raw.starts_with(&format!(":{X} {P} ")), "shows as a mode change:\n{raw}");
    assert!(!is_exec(&out.join("bin/run")));
    // And it resumes: nothing new to export, mirror untouched.
    let again = export_with(None, &store_hist, &out, &["--incremental"]);
    assert!(again.status.success(), "{}", combined(&again));
}
