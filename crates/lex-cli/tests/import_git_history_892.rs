//! #892 PR 5: `lex op import-git <repo|url>` — full first-parent history,
//! incremental re-import, the fold policy, URL sources and scale.
//!
//! Every fixture is a REAL git repo built with `git init/commit` under a
//! hermetic environment (no global/system config, pinned identities and dates
//! on EVERY commit/amend/merge), and every assertion drives the real `lex`
//! binary. The tip-only properties (publish parity, CRLF, non-Lex repos, limits)
//! live in `import_git_892.rs`; this file is the history half.
//!
//! The mutations each test was checked against (a one-line change to
//! `import_git*.rs` that must turn the named test red — see the PR description):
//!
//! * dropping fold recording (`state.pending_folded.push`) → the proving history
//! * ignoring the watermark (start at commit 0) → the re-import tests
//! * removing the first-parent-chain membership check → the rewritten-history test
//! * reading every manifest file per commit → the perf-shape test
//! * dropping `|| state.dirty` (skip the semantic pass on a commit that touches
//!   no Lex path even after a fold) → the proving history (`c6b`)

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_store::Store;
use lex_vcs::{IntentLog, OpLog, OperationRecord};
use tempfile::{tempdir, TempDir};

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── hermetic git ────────────────────────────────────────────────────────────

const ENV_KEYS: [&str; 9] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_AUTHOR_DATE",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_COMMITTER_DATE",
];

/// `git` with the developer's config, identity and dates kept out.
fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for v in ENV_KEYS {
        c.env_remove(v);
    }
    c.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file");
    c
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string()
}

/// A git command that creates commits: identities and dates pinned, so the
/// result never depends on the machine (a runner has no ambient identity).
fn git_id(dir: &Path, who: (&str, &str, &str), args: &[&str]) -> String {
    let out = git_cmd(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", who.0)
        .env("GIT_AUTHOR_EMAIL", who.1)
        .env("GIT_AUTHOR_DATE", who.2)
        .env("GIT_COMMITTER_NAME", who.0)
        .env("GIT_COMMITTER_EMAIL", who.1)
        .env("GIT_COMMITTER_DATE", who.2)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Author and committer differ, zones differ, and every commit gets its own
/// author/time so a lost or mixed-up provenance shows.
fn identity(n: usize) -> ((String, String, String), (String, String, String)) {
    let day = 1 + (n % 27);
    let a = (
        format!("Author {n}"),
        format!("author{n}@example.org"),
        format!("2024-03-{day:02}T10:{:02}:30+0530", n % 60),
    );
    let c = (
        format!("Committer {n}"),
        format!("committer{n}@example.org"),
        format!("2024-03-{day:02}T23:{:02}:01-0700", n % 60),
    );
    (a, c)
}

static COMMIT_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

fn commit(dir: &Path, msg: &str) -> String {
    let n = COMMIT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (a, c) = identity(n);
    let out = git_cmd(dir)
        .args(["commit", "-q", "--allow-empty", "--cleanup=verbatim", "-m", msg])
        .env("GIT_AUTHOR_NAME", &a.0)
        .env("GIT_AUTHOR_EMAIL", &a.1)
        .env("GIT_AUTHOR_DATE", &a.2)
        .env("GIT_COMMITTER_NAME", &c.0)
        .env("GIT_COMMITTER_EMAIL", &c.1)
        .env("GIT_COMMITTER_DATE", &c.2)
        .output()
        .unwrap();
    assert!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
    git(dir, &["rev-parse", "HEAD"])
}

/// Rewrite the tip's message (same tree): a new sha for the same change.
fn amend(dir: &Path, msg: &str) -> String {
    git_id(
        dir,
        ("Rewriter", "rewriter@example.org", "2024-06-01T00:00:00+0000"),
        &["commit", "-q", "--amend", "--allow-empty", "--cleanup=verbatim", "-m", msg],
    );
    git(dir, &["rev-parse", "HEAD"])
}

fn init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
}

fn write(dir: &Path, name: &str, contents: &[u8]) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, contents).unwrap();
}

fn add_all(dir: &Path) {
    git(dir, &["add", "-A"]);
}

fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn tracked(dir: &Path) -> Vec<String> {
    git(dir, &["ls-files"]).lines().map(str::to_string).collect()
}

// ── running the real binary ────────────────────────────────────────────────

struct Env {
    home: TempDir,
}

impl Env {
    fn new() -> Env {
        Env { home: tempdir().unwrap() }
    }
    fn lex(&self, args: &[&str]) -> Output {
        Command::new(lex_bin())
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("LEX_PACKAGES_DIR", self.home.path().join("packages"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("LEX_STORE")
            .env_remove("LEXHUB_TOKEN")
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Output {
        let o = self.lex(args);
        assert!(
            o.status.success(),
            "`lex {}` failed:\nstdout: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        o
    }
    fn json_ok(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--output", "json"];
        full.extend_from_slice(args);
        let o = self.ok(&full);
        serde_json::from_slice(&o.stdout).expect("json output")
    }
    /// `lex --output json op import-git <src> --store <store> <extra..>`: a FULL
    /// history import.
    fn import(&self, src: &str, store: &Path, extra: &[&str]) -> (Output, serde_json::Value) {
        let mut args = vec!["--output", "json", "op", "import-git", src, "--store", store.to_str().unwrap()];
        args.extend_from_slice(extra);
        let o = self.lex(&args);
        let v = serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
        (o, v)
    }
    fn import_repo(&self, repo: &Path, store: &Path, extra: &[&str]) -> (Output, serde_json::Value) {
        self.import(repo.to_str().unwrap(), store, extra)
    }
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn all_output(o: &Output) -> String {
    format!("{}{}", stdout(o), stderr(o))
}
fn imported_shas(v: &serde_json::Value) -> Vec<String> {
    data(v)["imported"].as_array().unwrap().iter().map(|i| i["sha"].as_str().unwrap().to_string()).collect()
}
fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

// ── store readers ───────────────────────────────────────────────────────────

fn head(store: &Path, branch: &str) -> Option<String> {
    Store::open(store).unwrap().get_branch(branch).unwrap().and_then(|b| b.head_op)
}

fn records(store: &Path, branch: &str) -> Vec<OperationRecord> {
    let h = head(store, branch).expect("branch has a head");
    OpLog::open(store).unwrap().walk_forward(&h, None).unwrap()
}

fn op_ids(store: &Path, branch: &str) -> Vec<String> {
    records(store, branch).into_iter().map(|r| r.op_id).collect()
}

fn branches(store: &Path) -> Vec<String> {
    let s = Store::open(store).unwrap();
    let mut b = s.list_branches().unwrap();
    b.retain(|n| s.get_branch(n).unwrap().is_some());
    b
}

fn manifest_id(store: &Path, branch: &str) -> Option<String> {
    Store::open(store).unwrap().branch_manifest(branch).unwrap().manifest().cloned()
}

fn sig_map(store: &Path, branch: &str) -> BTreeMap<String, String> {
    Store::open(store).unwrap().branch_head(branch).unwrap()
}

/// What one imported commit became in the op-log: its op kinds, oldest first,
/// and the origin its intent carries.
struct CommitOps {
    commit: String,
    kinds: Vec<String>,
    origin: lex_vcs::Origin,
    prompt: String,
}

fn ops_by_commit(store: &Path, branch: &str) -> Vec<CommitOps> {
    let intents = IntentLog::open(store).unwrap();
    let mut out: Vec<CommitOps> = Vec::new();
    for r in records(store, branch) {
        let intent = intents.get(r.op.intent_id.as_ref().expect("imported ops carry an intent")).unwrap().unwrap();
        let origin = intent.origin.clone().expect("imported intents carry an origin");
        let kind = serde_json::to_value(&r.op.kind).unwrap()["op"].as_str().unwrap().to_string();
        match out.last_mut() {
            Some(last) if last.commit == origin.commit => last.kinds.push(kind),
            _ => out.push(CommitOps { commit: origin.commit.clone(), kinds: vec![kind], origin, prompt: intent.prompt }),
        }
    }
    out
}

fn count(kinds: &[String], k: &str) -> usize {
    kinds.iter().filter(|x| *x == k).count()
}

// ── fixtures ────────────────────────────────────────────────────────────────

const LEX_TOML: &[u8] = b"[package]\nname = \"histpkg\"\nversion = \"0.1.0\"\n";
const MAIN_1: &str = "fn one() -> Int { 1 }\n";
const MAIN_2: &str = "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n";
const MAIN_3: &str = "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\nfn three() -> Int { 3 }\n";
const MAIN_BAD: &str =
    "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\nfn three() -> Int { 3 }\nfn bad() -> Int { \"not an int\" }\n";
const MAIN_7: &str =
    "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\nfn three() -> Int { 3 }\nfn seven() -> Int { 7 }\n";
const UTIL_1: &str = "fn helper() -> Int { 10 }\n";
const UTIL_2: &str = "fn helper() -> Int { 10 }\nfn helper2() -> Int { 11 }\n";
const SIDE_LEX: &str = "fn side_fn() -> Int { 40 }\n";
const REBORN: &str = "fn reborn() -> Int { 12 }\n";

/// The design's proving history (#892 §6, PR 5): a merge, a non-`.lex`-only
/// commit, an empty commit, a type-error commit that FOLDS (and a following
/// README-only commit that must fold with it), a deleted `.lex` file, a moved
/// file, an exec-bit change, a whole-package deletion, and the package coming
/// back.
struct Proving {
    c: BTreeMap<&'static str, String>,
}

///
/// `full` includes two shapes the ROUND-TRIP test cannot pass through today, for
/// reasons in code this PR does not touch (see the PR description): a moved
/// `.lex` file (`push` → `pull` → `export-git` of ANY history with one fails with
/// "unsatisfiable head entry (#992)", with `lex publish` alone and no importer
/// involved) and an exec bit DROPPED from an existing file (`export-git` only
/// ever sets the bit). With `full = false` the `.lex` file stays put and c10
/// ADDS an exec bit instead.
fn proving_repo(dir: &Path, full: bool) -> Proving {
    let mut c = BTreeMap::new();
    init(dir);
    write(dir, "lex.toml", LEX_TOML);
    write(dir, "src/main.lex", MAIN_1.as_bytes());
    write(dir, "src/util.lex", UTIL_1.as_bytes());
    write(dir, "README.md", b"# hist\n");
    write(dir, "bin/run.sh", b"#!/bin/sh\necho hi\n");
    chmod(&dir.join("bin/run.sh"), 0o755);
    write(dir, "docs/notes.txt", b"notes\n");
    add_all(dir);
    c.insert("c1", commit(dir, "c1 initial"));

    write(dir, "src/main.lex", MAIN_2.as_bytes());
    add_all(dir);
    c.insert("c2", commit(dir, "c2 add two"));

    // Non-.lex-only.
    write(dir, "README.md", b"# hist\n\nreadme edit\n");
    add_all(dir);
    c.insert("c3", commit(dir, "c3 readme only"));

    // No net change.
    c.insert("empty", commit(dir, "empty no net change"));

    // A side branch, and main moving on, then a --no-ff merge.
    git(dir, &["checkout", "-q", "-b", "side"]);
    write(dir, "src/side.lex", SIDE_LEX.as_bytes());
    write(dir, "SIDE.md", b"side\n");
    add_all(dir);
    c.insert("c4", commit(dir, "side work"));
    git(dir, &["checkout", "-q", "main"]);
    write(dir, "src/main.lex", MAIN_3.as_bytes());
    add_all(dir);
    c.insert("c5", commit(dir, "c5 add three"));
    let n = COMMIT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (a, _) = identity(n);
    git_id(dir, (a.0.as_str(), a.1.as_str(), a.2.as_str()), &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    c.insert("merge", git(dir, &["rev-parse", "HEAD"]));

    // A commit that does not type-check, that ALSO changes another module and a
    // manifest file: all of it must ride into the next importable commit.
    write(dir, "src/main.lex", MAIN_BAD.as_bytes());
    write(dir, "src/util.lex", UTIL_2.as_bytes());
    write(dir, "README.md", b"# hist\n\nreadme edit\n\nfrom the bad commit\n");
    add_all(dir);
    c.insert("c6", commit(dir, "c6 type error"));
    // The tree STILL holds the broken source: a README-only commit on top of it
    // must not land as a `SetFiles` over a stale semantic head.
    write(dir, "docs/notes.txt", b"notes v2\n");
    add_all(dir);
    c.insert("c6b", commit(dir, "c6b docs only, on a broken tree"));
    write(dir, "src/main.lex", MAIN_7.as_bytes());
    add_all(dir);
    c.insert("c7", commit(dir, "c7 fix"));

    // A deleted .lex file.
    git(dir, &["rm", "-q", "src/util.lex"]);
    c.insert("c8", commit(dir, "c8 delete util.lex"));

    // Moved files: a .lex file (remove + add of its declarations) and a plain one.
    git(dir, &["mv", "docs/notes.txt", "docs/guide.txt"]);
    if full {
        git(dir, &["mv", "src/side.lex", "src/extras.lex"]);
    }
    c.insert("c9", commit(dir, "c9 move files"));

    // An exec-bit-only change.
    if full {
        chmod(&dir.join("bin/run.sh"), 0o644);
        git(dir, &["update-index", "--chmod=-x", "bin/run.sh"]);
    } else {
        chmod(&dir.join("docs/guide.txt"), 0o755);
        git(dir, &["update-index", "--chmod=+x", "docs/guide.txt"]);
    }
    c.insert("c10", commit(dir, "c10 exec bit change"));

    // The whole package goes away.
    git(dir, &["rm", "-r", "-q", "src", "lex.toml"]);
    c.insert("c11", commit(dir, "c11 delete the package"));

    // ...and comes back.
    write(dir, "lex.toml", LEX_TOML);
    write(dir, "src/main.lex", REBORN.as_bytes());
    add_all(dir);
    c.insert("c12", commit(dir, "c12 package returns"));
    Proving { c }
}

/// A small linear Lex package history of `n` commits: commit `i` adds `fn f<i>`.
fn linear_repo(dir: &Path, n: usize) -> Vec<String> {
    init(dir);
    write(dir, "lex.toml", LEX_TOML);
    write(dir, "README.md", b"# linear\n");
    let mut shas = Vec::new();
    let mut src = String::new();
    for i in 1..=n {
        src.push_str(&format!("fn f{i}() -> Int {{ {i} }}\n"));
        write(dir, "src/main.lex", src.as_bytes());
        add_all(dir);
        shas.push(commit(dir, &format!("linear {i}")));
    }
    shas
}

fn add_linear_commits(dir: &Path, from: usize, to: usize) -> Vec<String> {
    let mut src = std::fs::read_to_string(dir.join("src/main.lex")).unwrap();
    let mut shas = Vec::new();
    for i in from..=to {
        src.push_str(&format!("fn f{i}() -> Int {{ {i} }}\n"));
        write(dir, "src/main.lex", src.as_bytes());
        add_all(dir);
        shas.push(commit(dir, &format!("linear {i}")));
    }
    shas
}

// ═══════════════════════════════════════════════════════════════════════════
// The proving history.
// ═══════════════════════════════════════════════════════════════════════════

/// One repo, every history shape the design lists, and per-commit expectations
/// read back from the report AND from the op-log.
///
/// Mutations: dropping `state.pending_folded.push(..)` empties `origin.folded`
/// and `folded_into`; dropping `|| state.dirty` lets `c6b` land as a
/// `SetFiles` on top of a semantic head that lacks the broken commit's changes.
#[test]
fn the_proving_history_imports_folds_and_converges() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let p = proving_repo(&repo, true);
    let c = |k: &str| p.c[k].clone();

    let store = t.path().join("store");
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    let d = data(&v);

    // ── the report ──────────────────────────────────────────────────────────
    assert_eq!(d["tip"]["sha"], c("c12").as_str());
    assert_eq!(d["tip"]["landed"], true);
    assert_eq!(d["mode"], "history");
    assert_eq!(d["shallow"], false);
    let want: Vec<String> =
        ["c1", "c2", "c3", "c5", "merge", "c7", "c8", "c9", "c10", "c11", "c12"].iter().map(|k| c(k)).collect();
    assert_eq!(imported_shas(&v), want, "first-parent order, side commit `c4` absent, folded/empty absent");
    assert_eq!(strings(&d["noop"]), vec![c("empty")], "the no-net-change commit is a noop");
    let folded = d["folded"].as_array().unwrap();
    assert_eq!(folded.len(), 2, "{folded:?}");
    assert_eq!(folded[0]["sha"], c("c6").as_str());
    assert_eq!(folded[0]["phase"], "type-check");
    assert!(!folded[0]["diagnostics"].as_array().unwrap().is_empty());
    assert_eq!(folded[1]["sha"], c("c6b").as_str());
    assert_eq!(folded[1]["phase"], "type-check", "the tree was still broken: it folds too");
    for f in folded {
        assert_eq!(f["folded_into"], c("c7").as_str());
    }
    assert_eq!(d["stats"]["commits"], 14);
    assert_eq!(d["stats"]["folded"], 2);
    assert!((d["stats"]["fold_ratio"].as_f64().unwrap() - 2.0 / 14.0).abs() < 1e-9);
    assert_eq!(head(&store, "main").as_deref(), d["head_op"].as_str());

    // ── the op-log, per commit ──────────────────────────────────────────────
    let per = ops_by_commit(&store, "main");
    assert_eq!(per.iter().map(|x| x.commit.clone()).collect::<Vec<_>>(), want);
    let by: BTreeMap<String, &CommitOps> = per.iter().map(|x| (x.commit.clone(), x)).collect();
    let k = |name: &str| by[&c(name)].kinds.clone();

    let c1 = k("c1");
    assert_eq!(count(&c1, "add_function"), 2, "one, helper: {c1:?}");
    assert_eq!(c1.last().unwrap(), "set_files");
    assert_eq!(k("c2"), vec!["add_function"], "a .lex-only commit changes no manifest file: no SetFiles");
    assert_eq!(k("c3"), vec!["set_files"], "a non-.lex-only commit is a SetFiles-only op");
    assert_eq!(k("c5"), vec!["add_function"]);
    assert_eq!(k("merge"), vec!["add_function", "set_files"], "the side branch's .lex + SIDE.md, in one intent");
    let m = by[&c("merge")];
    assert_eq!(m.origin.parents, vec![c("c5"), c("c4")], "every parent, first-parent first");
    assert!(m.origin.folded.is_empty());
    let merged = d["imported"].as_array().unwrap().iter().find(|i| i["sha"] == c("merge").as_str()).unwrap();
    assert_eq!(merged["merged"][0]["sha"], c("c4").as_str());
    assert_eq!(merged["merged"][0]["subject"], "side work", "the merged parent's subject line is reported");

    // The fold: c6's .lex change (util helper2), c7's own (seven), and the
    // manifest changes of c6 + c6b + c7, all in c7's ops; c7's origin names both.
    let c7 = k("c7");
    assert_eq!(count(&c7, "add_function"), 2, "helper2 (from c6) and seven (c7's): {c7:?}");
    assert_eq!(c7.last().unwrap(), "set_files");
    assert_eq!(by[&c("c7")].origin.folded, vec![c("c6"), c("c6b")], "oldest first, hashed into the intent");
    assert_eq!(by[&c("c7")].prompt, "c7 fix\n", "the message verbatim, trailing newline included");
    for other in ["c1", "c2", "c3", "c5", "merge", "c8", "c9", "c10", "c11", "c12"] {
        assert!(by[&c(other)].origin.folded.is_empty(), "{other}");
    }

    assert_eq!(k("c8"), vec!["remove_function", "remove_function"], "a deleted .lex file: helper, helper2");
    assert_eq!(
        k("c9"),
        vec!["rename_symbol", "set_files"],
        "a moved .lex file is a rename of its declaration (as `lex publish` sees it); the moved plain file a manifest change"
    );
    assert_eq!(k("c10"), vec!["set_files"], "an exec-bit change is a manifest-only change");
    let c11 = k("c11");
    assert_eq!(count(&c11, "remove_function"), 5, "a whole-package deletion removes every declaration: {c11:?}");
    assert_eq!(c11.last().unwrap(), "set_files", "lex.toml leaves the manifest");
    let c12 = k("c12");
    assert_eq!(c12, vec!["add_function", "set_files"]);
    // The report's per-commit op counts match the op-log.
    for i in d["imported"].as_array().unwrap() {
        assert_eq!(i["ops"].as_u64().unwrap() as usize, by[i["sha"].as_str().unwrap()].kinds.len());
    }
    // What was skipped: the empty commit and both folded ones left no intent of their own.
    for gone in ["empty", "c4", "c6", "c6b"] {
        assert!(!by.contains_key(&c(gone)), "{gone} has no ops of its own");
    }

    // ── convergence: the same head state as a head-only import of the tip ───
    let snap = t.path().join("snapshot");
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--store", snap.to_str().unwrap()]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(sig_map(&store, "main"), sig_map(&snap, "main"), "same sig -> stage map");
    assert_eq!(sig_map(&store, "main").len(), 1, "only `reborn` is live at the tip");
    assert_eq!(manifest_id(&store, "main"), manifest_id(&snap, "main"), "same manifest id");
    assert!(manifest_id(&store, "main").is_some());
    assert_ne!(head(&store, "main"), head(&snap, "main"), "the lineages differ; the STATE converges");

    // The source's own contents at the tip, from the manifest.
    let s = Store::open(&store).unwrap();
    let man = s.get_manifest(&manifest_id(&store, "main").unwrap()).unwrap();
    let paths: Vec<&str> = man.entries.keys().map(String::as_str).collect();
    assert_eq!(paths, vec!["README.md", "SIDE.md", "bin/run.sh", "docs/guide.txt", "lex.toml"]);
    assert_eq!(man.entries["bin/run.sh"].mode, "100644", "the exec bit was dropped by c10");
}

/// A run over the proving history also leaves a local report file that equals
/// what `--output json` printed (a convenience copy, never a source of truth).
#[test]
fn the_report_is_also_written_beside_the_store() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    linear_repo(&repo, 3);
    let store = t.path().join("store");
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success());
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.join("import/main.json")).unwrap()).unwrap();
    assert_eq!(&file, data(&v));
    assert_eq!(branches(&store), vec!["main".to_string()], "the report dir is not a branch");
}

// ═══════════════════════════════════════════════════════════════════════════
// Round trip: import → push → pull → export-git.
// ═══════════════════════════════════════════════════════════════════════════

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
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    loop {
        assert!(std::time::Instant::now() < deadline, "test server never became ready within 10s");
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    (Server { addr, _join: Some(join) }, tmp)
}

fn is_lex_path(p: &str) -> bool {
    p == "src.lex" || (p.starts_with("src/") && p.ends_with(".lex"))
}

/// The canonical AST of a `.lex` file, order-normalized (the op-log renders
/// declarations in sig order, not source order).
fn canonical_ast(env: &Env, path: &Path) -> serde_json::Value {
    let v = env.json_ok(&["parse", path.to_str().unwrap()]);
    let mut d = data(&v).clone();
    if let Some(arr) = d.as_array_mut() {
        arr.sort_by_key(|e| e.get("name").and_then(|n| n.as_str()).map(String::from).unwrap_or_else(|| e.to_string()));
    }
    d
}

/// Import the FULL proving history, push it to a real in-process hub, pull it
/// into a fresh store, `export-git` — and the exported log has the ORIGINAL
/// author/committer/date/timezone/message of every imported commit (with its
/// `Git-Source` trailer), in order, and the final tree equals the source tip's
/// tree modulo the accepted differences (formatting of `.lex`, history shape).
#[test]
fn full_history_round_trips_through_the_hub_and_export_git() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let p = proving_repo(&repo, false);

    let a = t.path().join("store-a");
    let (o, v) = env.import_repo(&repo, &a, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    let imported = imported_shas(&v);
    assert_eq!(imported.len(), 11);

    let (server, _hub) = start_server();
    let hub = format!("http://{}", server.addr);
    env.json_ok(&["op", "push", &hub, "--store", a.to_str().unwrap()]);
    let b = t.path().join("store-b");
    env.json_ok(&["op", "pull", &hub, "--store", b.to_str().unwrap()]);
    assert_eq!(op_ids(&a, "main"), op_ids(&b, "main"), "the pulled lineage is the pushed one");

    let out = t.path().join("exported");
    env.ok(&["export-git", out.to_str().unwrap(), "--store", b.to_str().unwrap(), "--branch", "main"]);

    // One git commit per imported commit, oldest first, with the original provenance.
    assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]), imported.len().to_string());
    let fmt = "--format=%an|%ae|%aI|%cn|%ce|%cI";
    let exported: Vec<String> = git(&out, &["rev-list", "--reverse", "HEAD"]).lines().map(str::to_string).collect();
    for (src_sha, exp_sha) in imported.iter().zip(&exported) {
        assert_eq!(
            git(&out, &["log", "-1", fmt, exp_sha]),
            git(&repo, &["log", "-1", fmt, src_sha]),
            "author/committer/date/tz of {src_sha}"
        );
        let want_subject = git(&repo, &["log", "-1", "--format=%s", src_sha]);
        let body = git(&out, &["log", "-1", "--format=%B", exp_sha]);
        assert!(body.starts_with(&want_subject), "{body:?} vs {want_subject:?}");
        assert!(body.contains(&format!("Git-Source: {src_sha}")), "{body:?}");
    }
    assert!(git(&out, &["log", "-1", "--format=%B", exported.last().unwrap()]).contains(&p.c["c12"]));

    // The final tree equals the source tip's tree, modulo the accepted differences.
    let (src_files, out_files) = (tracked(&repo), tracked(&out));
    assert_eq!(src_files, out_files, "same file set");
    for f in &src_files {
        if is_lex_path(f) {
            assert_eq!(canonical_ast(&env, &repo.join(f)), canonical_ast(&env, &out.join(f)), "{f}: AST-equal");
        } else {
            assert_eq!(std::fs::read(repo.join(f)).unwrap(), std::fs::read(out.join(f)).unwrap(), "{f} byte-identical");
        }
    }
    // Modes are compared where git records them: the committed trees.
    let modes = |d: &Path| git(d, &["ls-tree", "-r", "HEAD"]).lines().map(|l| l.split(' ').next().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(modes(&repo), modes(&out), "committed file modes");
}

// ═══════════════════════════════════════════════════════════════════════════
// Determinism.
// ═══════════════════════════════════════════════════════════════════════════

/// Two independent full imports into two fresh stores (a second process, later,
/// a different store-branch name) give the same head OpId AND the same OpId list;
/// so does an import from a clone at a different path.
#[test]
fn twin_full_imports_and_a_clone_converge_on_every_op_id() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    proving_repo(&repo, true);

    let (a, b, c) = (t.path().join("a"), t.path().join("b"), t.path().join("c"));
    assert!(env.import_repo(&repo, &a, &[]).0.status.success());
    thread::sleep(Duration::from_millis(1100));
    assert!(env.import_repo(&repo, &b, &["--store-branch", "renamed"]).0.status.success());
    assert_eq!(head(&a, "main"), head(&b, "renamed"), "identical repo => identical head OpId");
    assert_eq!(op_ids(&a, "main"), op_ids(&b, "renamed"), "and identical ops, in order");
    assert!(op_ids(&a, "main").len() > 20, "a non-trivial lineage: {}", op_ids(&a, "main").len());

    let clone = t.path().join("elsewhere").join("clone");
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(t.path(), &["clone", "-q", repo.to_str().unwrap(), clone.to_str().unwrap()]);
    assert!(env.import_repo(&clone, &c, &[]).0.status.success());
    assert_eq!(op_ids(&c, "main"), op_ids(&a, "main"), "repo identity is the root sha, not the path");
}

// ═══════════════════════════════════════════════════════════════════════════
// Incremental re-import: the watermark is the op-log.
// ═══════════════════════════════════════════════════════════════════════════

/// Re-running with nothing new is 0 ops and exit 0; after two new source
/// commits the re-import adds exactly those two; and the resulting lineage is
/// op-for-op the one a single full import of the final repo would have made.
///
/// Mutation: ignoring the watermark (starting at commit 0) re-imports (or
/// re-refuses) everything, so the "nothing new" and "exactly two" assertions fail.
#[test]
fn reimport_is_incremental_and_equals_a_full_import() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let shas = linear_repo(&repo, 3);
    let store = t.path().join("store");

    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), shas);
    let (h1, n1) = (head(&store, "main"), op_ids(&store, "main").len());

    // Nothing new.
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), Vec::<String>::new());
    assert_eq!(data(&v)["tip"]["landed"], true);
    assert_eq!(data(&v)["watermark"], shas[2].as_str());
    assert_eq!(data(&v)["stats"]["commits"], 0);
    assert_eq!(head(&store, "main"), h1, "0 new ops");
    assert_eq!(op_ids(&store, "main").len(), n1);
    assert_eq!(branches(&store), vec!["main".to_string()], "no work branch left behind");

    // Two more commits.
    let more = add_linear_commits(&repo, 4, 5);
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), more, "exactly the new commits");
    assert_eq!(data(&v)["watermark"], shas[2].as_str());
    assert_eq!(op_ids(&store, "main").len(), n1 + 2, "one AddFunction each (main.lex is the only changed file)");

    // The extended lineage is byte-for-byte what a fresh full import makes.
    let fresh = t.path().join("fresh");
    assert!(env.import_repo(&repo, &fresh, &[]).0.status.success());
    assert_eq!(op_ids(&store, "main"), op_ids(&fresh, "main"));
    assert_eq!(head(&store, "main"), head(&fresh, "main"));
}

/// A rewritten (amended) already-imported commit is refused with the
/// `--store-branch` instruction and the store is unchanged; importing into a new
/// branch works.
///
/// Mutation: removing the first-parent-chain membership check lets the rewritten
/// history through and this test red.
#[test]
fn rewritten_history_is_refused_and_the_store_is_untouched() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let shas = linear_repo(&repo, 3);
    let store = t.path().join("store");
    assert!(env.import_repo(&repo, &store, &[]).0.status.success());
    let (h, ops) = (head(&store, "main"), op_ids(&store, "main"));

    // Amend the already-imported tip: same tree, new sha. Then add a commit on top.
    let new_tip = amend(&repo, "linear 3 (reworded)");
    assert_ne!(new_tip, shas[2]);
    add_linear_commits(&repo, 4, 4);

    let (o, _) = env.import_repo(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    let msg = all_output(&o);
    assert!(msg.contains("--store-branch"), "{msg}");
    assert!(msg.contains(&shas[2]), "names the commit it stopped at: {msg}");
    assert!(msg.contains("rewrite") || msg.contains("amended"), "{msg}");
    assert_eq!(head(&store, "main"), h, "the store is unchanged");
    assert_eq!(op_ids(&store, "main"), ops);
    assert_eq!(branches(&store), vec!["main".to_string()], "no work branch left behind");

    // The instruction works: a new branch imports the rewritten history in full.
    let (o, v) = env.import_repo(&repo, &store, &["--store-branch", "rewritten"]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v).len(), 4);
    assert_eq!(head(&store, "main"), h, "the original branch is still untouched");
}

/// Native (non-imported) ops after the last imported one refuse the import
/// (suggesting a separate branch + merge); a non-empty branch with no origin at
/// all is refused too.
#[test]
fn native_ops_and_originless_branches_are_refused() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    linear_repo(&repo, 2);
    let store = t.path().join("store");
    assert!(env.import_repo(&repo, &store, &[]).0.status.success());

    // A native publish on top of the imported lineage.
    let native = t.path().join("native");
    write(&native, "lex.toml", b"[package]\nname = \"nativepkg\"\nversion = \"0.1.0\"\n");
    write(&native, "src/n.lex", b"fn n() -> Int { 5 }\n");
    env.ok(&[
        "publish",
        "--store",
        store.to_str().unwrap(),
        "--branch",
        "main",
        "--intent-prompt",
        "native work",
        "--intent-session",
        "native-session",
        native.to_str().unwrap(),
    ]);
    let h = head(&store, "main");
    add_linear_commits(&repo, 3, 3);
    let (o, _) = env.import_repo(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    let msg = all_output(&o);
    assert!(msg.contains("native") && msg.contains("--store-branch") && msg.contains("merge"), "{msg}");
    assert_eq!(head(&store, "main"), h, "unchanged");

    // A branch that only ever held native ops (a fresh store, published to natively).
    let nat = t.path().join("nat-store");
    env.ok(&[
        "publish",
        "--store",
        nat.to_str().unwrap(),
        "--branch",
        "main",
        "--intent-prompt",
        "only native",
        "--intent-session",
        "native-session",
        native.to_str().unwrap(),
    ]);
    assert!(head(&nat, "main").is_some());
    let (o, _) = env.import_repo(&repo, &nat, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    assert!(all_output(&o).contains("none of it came from an import"), "{}", all_output(&o));
}

/// A branch imported from a different repository (a different root sha) is
/// refused rather than extended: the OpIds could not converge.
#[test]
fn a_branch_from_a_different_repository_is_refused() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let (r1, r2) = (t.path().join("r1"), t.path().join("r2"));
    linear_repo(&r1, 2);
    // Same shape, different root commit (different message => different sha).
    init(&r2);
    write(&r2, "lex.toml", LEX_TOML);
    write(&r2, "src/main.lex", MAIN_1.as_bytes());
    add_all(&r2);
    commit(&r2, "a different root");
    let store = t.path().join("store");
    assert!(env.import_repo(&r1, &store, &[]).0.status.success());
    let h = head(&store, "main");
    let (o, _) = env.import_repo(&r2, &store, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    assert!(all_output(&o).contains("different repository"), "{}", all_output(&o));
    assert_eq!(head(&store, "main"), h);
}

/// `--head-only` needs an empty branch. After a `--head-only` snapshot a later
/// full import works forward from the snapshot — history before it is NOT
/// backfilled, and the report says so.
#[test]
fn head_only_then_full_works_forward_and_never_backfills() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let shas = linear_repo(&repo, 3);
    let store = t.path().join("store");

    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--store", store.to_str().unwrap()]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(op_ids(&store, "main").len(), 4, "3 AddFunction (a snapshot) + SetFiles");
    // --head-only on a non-empty branch is refused, even when it is up to date.
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--store", store.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(all_output(&o).contains("--store-branch"), "{}", all_output(&o));

    let more = add_linear_commits(&repo, 4, 5);
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), more, "only the commits after the snapshot");
    assert_eq!(data(&v)["watermark"], shas[2].as_str());
    assert_eq!(data(&v)["snapshot_base"]["commit"], shas[2].as_str());
    assert_eq!(data(&v)["snapshot_base"]["via"], "head-only");
    assert!(
        data(&v)["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("not backfilled")),
        "the report says earlier history is not backfilled: {}",
        data(&v)["notes"]
    );
    let per = ops_by_commit(&store, "main");
    assert_eq!(per.iter().map(|p| p.commit.clone()).collect::<Vec<_>>(), vec![shas[2].clone(), more[0].clone(), more[1].clone()]);
}

// ═══════════════════════════════════════════════════════════════════════════
// --on-error, --strict, the tip.
// ═══════════════════════════════════════════════════════════════════════════

/// A repo whose 3rd of 5 commits does not type-check.
fn broken_middle(dir: &Path) -> Vec<String> {
    let mut shas = linear_repo(dir, 2);
    let good = std::fs::read_to_string(dir.join("src/main.lex")).unwrap();
    write(dir, "src/main.lex", format!("{good}fn broken() -> Int {{ \"no\" }}\n").as_bytes());
    add_all(dir);
    shas.push(commit(dir, "the broken one"));
    write(dir, "src/main.lex", format!("{good}fn f4() -> Int {{ 4 }}\n").as_bytes());
    add_all(dir);
    shas.push(commit(dir, "fixed, plus f4"));
    write(dir, "src/main.lex", format!("{good}fn f4() -> Int {{ 4 }}\nfn f5() -> Int {{ 5 }}\n").as_bytes());
    add_all(dir);
    shas.push(commit(dir, "f5"));
    shas
}

/// `--on-error stop` halts at the first failing commit: exit 2, the earlier
/// commits stay imported (the store is valid but stale), the later ones are not
/// attempted. `fold` (the default) imports past it.
#[test]
fn on_error_stop_halts_at_the_first_failure_and_fold_goes_on() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let s = broken_middle(&repo);

    let stop = t.path().join("stop");
    let (o, v) = env.import_repo(&repo, &stop, &["--on-error", "stop"]);
    assert_eq!(o.status.code(), Some(2), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), vec![s[0].clone(), s[1].clone()], "earlier commits stay imported");
    assert_eq!(data(&v)["tip"]["landed"], false);
    assert_eq!(data(&v)["tip"]["sha"], s[4].as_str());
    assert_eq!(data(&v)["tip"]["failed_commit"], s[2].as_str());
    assert_eq!(data(&v)["tip"]["phase"], "type-check");
    assert_eq!(data(&v)["stats"]["commits"], 3, "the later commits were not attempted");
    assert_eq!(data(&v)["folded"].as_array().unwrap().len(), 0);
    assert_eq!(op_ids(&stop, "main").len(), 3, "f1, f2 and the SetFiles");
    assert_eq!(branches(&stop), vec!["main".to_string()]);
    // Extending later (after the source is repaired upstream, or not) keeps going from the watermark.
    let (o, v) = env.import_repo(&repo, &stop, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), vec![s[3].clone(), s[4].clone()]);
    assert_eq!(data(&v)["folded"].as_array().unwrap()[0]["sha"], s[2].as_str());

    let fold = t.path().join("fold");
    let (o, v) = env.import_repo(&repo, &fold, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(imported_shas(&v), vec![s[0].clone(), s[1].clone(), s[3].clone(), s[4].clone()]);
    let folded: Vec<String> =
        data(&v)["folded"].as_array().unwrap().iter().map(|f| f["sha"].as_str().unwrap().to_string()).collect();
    assert_eq!(folded, vec![s[2].clone()]);
    // Stop-then-continue and fold-in-one-go reach the same head state.
    assert_eq!(sig_map(&stop, "main"), sig_map(&fold, "main"));
    assert_eq!(manifest_id(&stop, "main"), manifest_id(&fold, "main"));
}

/// When the TIP itself is refused under `fold`, exit is 2, the earlier commits
/// are imported, and the tip's phase and diagnostics are in the report.
#[test]
fn a_refused_tip_exits_2_but_earlier_commits_stay() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let s = linear_repo(&repo, 2);
    let good = std::fs::read_to_string(repo.join("src/main.lex")).unwrap();
    write(&repo, "src/main.lex", format!("{good}fn bad() -> Int {{ \"x\" }}\n").as_bytes());
    add_all(&repo);
    let bad = commit(&repo, "bad tip");

    let store = t.path().join("store");
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), s);
    assert_eq!(data(&v)["tip"]["sha"], bad.as_str());
    assert_eq!(data(&v)["tip"]["landed"], false);
    assert_eq!(data(&v)["tip"]["phase"], "type-check");
    assert!(!data(&v)["tip"]["diagnostics"].as_array().unwrap().is_empty());
    assert!(head(&store, "main").is_some(), "the store is valid, stale at the last good commit");
    assert_eq!(sig_map(&store, "main").len(), 2);
}

/// A symlink that persists across commits is listed ONCE (first-seen and
/// last-seen commits), not once per commit; `--strict` refuses the first commit
/// that carries it and lands nothing.
#[test]
fn a_persistent_symlink_is_listed_once_and_strict_refuses_it() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "a.txt", b"1\n");
    std::os::unix::fs::symlink("a.txt", repo.join("link")).unwrap();
    add_all(&repo);
    let c1 = commit(&repo, "with a symlink");
    write(&repo, "a.txt", b"2\n");
    add_all(&repo);
    commit(&repo, "edit");
    write(&repo, "a.txt", b"3\n");
    add_all(&repo);
    let c3 = commit(&repo, "edit again");

    let store = t.path().join("store");
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{}", all_output(&o));
    let un = data(&v)["unsupported"].as_array().unwrap();
    assert_eq!(un.len(), 1, "{un:?}");
    assert_eq!((un[0]["path"].as_str(), un[0]["kind"].as_str()), (Some("link"), Some("symlink")));
    assert_eq!(un[0]["first_seen"], c1.as_str());
    assert_eq!(un[0]["last_seen"], c3.as_str());
    assert_eq!(un[0]["commit"], c1.as_str());

    let strict = t.path().join("strict");
    let (o, v) = env.import_repo(&repo, &strict, &["--strict"]);
    assert_eq!(o.status.code(), Some(2), "{}", all_output(&o));
    assert_eq!(data(&v)["tip"]["phase"], "strict");
    assert_eq!(head(&strict, "main"), None, "--strict lands nothing");
}

// ═══════════════════════════════════════════════════════════════════════════
// --since, --max-commits, --examples.
// ═══════════════════════════════════════════════════════════════════════════

/// `--max-commits N` imports the first N commits and reports the cut; the next
/// run continues from the watermark and ends up op-for-op where one full import
/// does. `--since` starts a lineage at a commit (a snapshot of it) and the head
/// STATE converges with the full import.
#[test]
fn max_commits_resumes_and_since_starts_a_lineage_at_a_commit() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let s = linear_repo(&repo, 5);

    let full = t.path().join("full");
    assert!(env.import_repo(&repo, &full, &[]).0.status.success());

    let part = t.path().join("part");
    let (o, v) = env.import_repo(&repo, &part, &["--max-commits", "2"]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), s[..2].to_vec());
    assert_eq!(data(&v)["truncated"], true);
    assert_eq!(data(&v)["remaining"], 3);
    assert_eq!(data(&v)["tip"]["sha"], s[1].as_str(), "the tip of THIS run is the last commit it was allowed");
    assert_eq!(data(&v)["requested_tip"], s[4].as_str());
    let (o, v) = env.import_repo(&repo, &part, &[]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), s[2..].to_vec());
    assert_eq!(op_ids(&part, "main"), op_ids(&full, "main"), "resumed == one-go");

    let since = t.path().join("since");
    let (o, v) = env.import_repo(&repo, &since, &["--since", &s[2]]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), s[2..].to_vec(), "starts AT the commit");
    assert_eq!(data(&v)["since"], s[2].as_str());
    assert_eq!(sig_map(&since, "main"), sig_map(&full, "main"));
    assert_eq!(manifest_id(&since, "main"), manifest_id(&full, "main"));
    assert_ne!(head(&since, "main"), head(&full, "main"), "a different lineage");
    // Bad values.
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--since", "deadbeef", "--store", since.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--max-commits", "0"]);
    assert_eq!(o.status.code(), Some(1));
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--since", &s[1]]);
    assert_eq!(o.status.code(), Some(1));
}

/// `--examples tip` gates only the final commit, `all` every importable commit,
/// `none` never. The type-check gate always runs.
#[test]
fn the_examples_policy_is_per_commit() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "lex.toml", LEX_TOML);
    // c1: a WRONG example (only the examples gate can refuse it). c2: fixed.
    write(&repo, "src/main.lex", b"fn double(n :: Int) -> Int\n  examples { double(2) => 5 }\n{\n  n * 2\n}\n");
    add_all(&repo);
    let c1 = commit(&repo, "wrong example");
    write(&repo, "src/main.lex", b"fn double(n :: Int) -> Int\n  examples { double(2) => 4 }\n{\n  n * 2\n}\n");
    add_all(&repo);
    let c2 = commit(&repo, "right example");

    // tip: c1 is not example-gated, so it imports; the tip is.
    let s1 = t.path().join("tip");
    let (o, v) = env.import_repo(&repo, &s1, &[]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), vec![c1.clone(), c2.clone()]);
    // all: c1 is gated and refused, then folds into c2.
    let s2 = t.path().join("all");
    let (o, v) = env.import_repo(&repo, &s2, &["--examples", "all"]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), vec![c2.clone()]);
    assert_eq!(data(&v)["folded"][0]["sha"], c1.as_str());
    assert_eq!(data(&v)["folded"][0]["phase"], "examples");
    // none.
    let s3 = t.path().join("none");
    let (o, v) = env.import_repo(&repo, &s3, &["--examples", "none"]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(imported_shas(&v), vec![c1, c2]);
}

// ═══════════════════════════════════════════════════════════════════════════
// URL sources, shallow repos.
// ═══════════════════════════════════════════════════════════════════════════

/// A `file://` URL imports exactly like the local path (same OpIds — the URL is
/// not part of any hash), the bare clone is gone afterwards, and the report
/// never carries the path/URL into an intent.
#[test]
fn a_file_url_import_equals_the_local_path_import() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    linear_repo(&repo, 4);
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    assert!(env.import_repo(&repo, &a, &[]).0.status.success());
    let url = format!("file://{}", repo.display());
    let (o, v) = env.import(&url, &b, &[]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(op_ids(&a, "main"), op_ids(&b, "main"));
    assert_eq!(data(&v)["source"]["kind"], "url");
    assert_eq!(data(&v)["shallow"], false, "a URL imports FULL history by default");
    assert_eq!(imported_shas(&v).len(), 4);
    // Nothing about the source location is hashed: the intents mention neither path nor URL.
    let intents = IntentLog::open(&b).unwrap();
    for r in records(&b, "main") {
        let i = intents.get(r.op.intent_id.as_ref().unwrap()).unwrap().unwrap();
        let s = serde_json::to_string(&i).unwrap();
        assert!(!s.contains(repo.to_str().unwrap()) && !s.contains("file://"), "{s}");
    }
    // --branch works through a URL, and --head-only.
    let c = t.path().join("c");
    let o = env.lex(&["op", "import-git", &url, "--head-only", "--branch", "main", "--store", c.to_str().unwrap()]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(sig_map(&c, "main"), sig_map(&a, "main"));
}

/// Userinfo in a URL never reaches the report or an error message. The fake URL
/// fails to clone (nothing listens on port 1) and its output must not contain
/// the password — nor make a network call to anywhere.
#[test]
fn url_userinfo_is_stripped_from_errors() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let store = t.path().join("store");
    let (o, _) = env.import("https://user:secret@127.0.0.1:1/x.git", &store, &[]);
    assert_eq!(o.status.code(), Some(1), "{}", all_output(&o));
    let all = all_output(&o);
    assert!(!all.contains("secret"), "the password leaked: {all}");
    assert!(!all.contains("user:"), "userinfo leaked: {all}");
    assert!(all.contains("127.0.0.1"), "the host is still named: {all}");
    assert_eq!(head(&store, "main"), None);
}

/// A shallow LOCAL repo is refused (its root is not the real root). From a URL,
/// `--depth N` is explicit: the shallow boundary becomes the lineage root, the
/// report says `shallow: true`, and the OpIds differ from a full import's
/// (a different session). `--depth` on a local path is an error.
#[test]
fn shallow_sources_are_refused_or_marked() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let shas = linear_repo(&repo, 4);
    let url = format!("file://{}", repo.display());

    let shallow = t.path().join("shallow");
    git(t.path(), &["clone", "-q", "--depth", "2", &url, shallow.to_str().unwrap()]);
    let (o, _) = env.import_repo(&shallow, &t.path().join("s0"), &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(all_output(&o).contains("shallow"), "{}", all_output(&o));

    let full = t.path().join("full");
    assert!(env.import_repo(&repo, &full, &[]).0.status.success());
    let s = t.path().join("s");
    let (o, v) = env.import(&url, &s, &["--depth", "2"]);
    assert!(o.status.success(), "{}", all_output(&o));
    assert_eq!(data(&v)["shallow"], true);
    assert_eq!(imported_shas(&v), shas[2..].to_vec(), "history before the shallow boundary is absent");
    assert!(
        data(&v)["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("shallow")),
        "{}",
        data(&v)["notes"]
    );
    let sessions = |store: &Path| -> String {
        let i = IntentLog::open(store).unwrap();
        i.get(records(store, "main")[0].op.intent_id.as_ref().unwrap()).unwrap().unwrap().session_id
    };
    assert_ne!(sessions(&s), sessions(&full), "the boundary is the root: a different lineage");
    assert_eq!(sessions(&s), format!("git-import:{}", shas[2]));
    assert_ne!(head(&s, "main"), head(&full, "main"));

    let (o, _) = env.import_repo(&repo, &t.path().join("s2"), &["--depth", "2"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(all_output(&o).contains("--depth"), "{}", all_output(&o));
}

// ═══════════════════════════════════════════════════════════════════════════
// Scale: the perf SHAPE (not timing).
// ═══════════════════════════════════════════════════════════════════════════

/// Per-commit blob reads are O(changed files), not O(head): a 30-file package
/// with one changed file per commit reads ~1 object per commit, an unchanged (or
/// reverted) blob is never re-read, and a commit that touches no Lex path skips
/// the semantic pass.
///
/// Mutation: rebuilding the manifest from every file's bytes each commit (or
/// reading every tracked blob) multiplies `blob_reads` by ~30 and reds this.
#[test]
fn per_commit_reads_are_proportional_to_changed_files_not_the_head() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "lex.toml", LEX_TOML);
    write(&repo, "README.md", b"# perf\n");
    for i in 0..28 {
        write(&repo, &format!("src/m{i:02}.lex"), format!("fn m{i}() -> Int {{ {i} }}\n").as_bytes());
    }
    write(&repo, "docs/a.txt", b"a\n");
    add_all(&repo);
    commit(&repo, "30 files"); // lex.toml + README + 28 src + docs/a.txt = 31 tracked paths
    assert_eq!(tracked(&repo).len(), 31);

    let mut lex_commits = 0;
    let mut text_commits = 0;
    for i in 0..12 {
        if i % 2 == 0 {
            // A .lex change.
            let n = i / 2;
            write(&repo, &format!("src/m{n:02}.lex"), format!("fn m{n}() -> Int {{ {} }}\n", 100 + i).as_bytes());
            lex_commits += 1;
        } else {
            // A text change; 9 flips docs/a.txt back to content it had before (a cache hit).
            let body: Vec<u8> = if i == 9 { b"a\n".to_vec() } else { format!("text {i}\n").into_bytes() };
            write(&repo, "docs/a.txt", &body);
            text_commits += 1;
        }
        add_all(&repo);
        commit(&repo, &format!("change {i}"));
    }
    assert_eq!((lex_commits, text_commits), (6, 6));

    let store = t.path().join("store");
    let (o, v) = env.import_repo(&repo, &store, &[]);
    assert!(o.status.success(), "{}", all_output(&o));
    let st = &data(&v)["stats"];
    assert_eq!(st["commits"], 13);
    let reads = st["blob_reads"].as_u64().unwrap();
    // The first commit reads each of its 31 objects once; every later commit
    // reads only what it changed (the reverted docs/a.txt is cached: 0 reads).
    assert!(reads >= 31, "the snapshot reads its files: {reads}");
    assert!(reads <= 31 + 12, "13 commits over 31 files must NOT be ~400 reads: {reads}");
    assert!(st["blob_cache_hits"].as_u64().unwrap() >= 1, "the reverted blob is a cache hit: {st}");
    // The semantic pass runs for the snapshot and the 6 .lex commits only.
    assert_eq!(st["semantic_passes"], 7, "{st}");
    assert_eq!(st["semantic_skipped"], 6, "{st}");
    assert_eq!(imported_shas(&v).len(), 13);
}

/// `#[ignore]`d benchmark: a synthetic 200-commit history over ~100 `.lex` files.
/// Prints timings; asserts a GENEROUS budget so it catches an O(n^2) regression,
/// not a slow runner. Run with:
///
/// `cargo test --release -p lex-cli --test import_git_history_892 -- --ignored --nocapture bench`
#[test]
#[ignore = "benchmark: run explicitly with --ignored --nocapture"]
fn bench_two_hundred_commits_over_a_hundred_lex_files() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "lex.toml", LEX_TOML);
    write(&repo, "README.md", b"# bench\n");
    for i in 0..100 {
        write(&repo, &format!("src/f{i:03}.lex"), format!("fn f{i}() -> Int {{ {i} }}\n").as_bytes());
    }
    add_all(&repo);
    commit(&repo, "100 files");
    let build = std::time::Instant::now();
    for i in 1..200 {
        if i % 5 == 0 {
            write(&repo, "README.md", format!("# bench {i}\n").as_bytes());
        } else {
            let k = i % 100;
            let mut s = std::fs::read_to_string(repo.join(format!("src/f{k:03}.lex"))).unwrap();
            s.push_str(&format!("fn f{k}_{i}() -> Int {{ {i} }}\n"));
            write(&repo, &format!("src/f{k:03}.lex"), s.as_bytes());
        }
        add_all(&repo);
        commit(&repo, &format!("commit {i}"));
    }
    println!("built the 200-commit repo in {:?}", build.elapsed());

    let store = t.path().join("store");
    let start = std::time::Instant::now();
    let (o, v) = env.import_repo(&repo, &store, &["--examples", "none"]);
    let wall = start.elapsed();
    assert!(o.status.success(), "{}", all_output(&o));
    let st = &data(&v)["stats"];
    println!("imported {} commits in {wall:?}; stats: {st}", imported_shas(&v).len());
    assert_eq!(st["commits"], 200);
    assert!(wall < Duration::from_secs(600), "200 commits took {wall:?}: an O(n^2) regression?");

    // Incremental: 5 more commits are cheap (no walk of the 200-commit history).
    for i in 200..205 {
        write(&repo, "README.md", format!("# bench {i}\n").as_bytes());
        add_all(&repo);
        commit(&repo, &format!("commit {i}"));
    }
    let start = std::time::Instant::now();
    let (o, v) = env.import_repo(&repo, &store, &["--examples", "none"]);
    assert!(o.status.success(), "{}", all_output(&o));
    println!("incremental 5 commits in {:?}; stats: {}", start.elapsed(), data(&v)["stats"]);
    assert_eq!(imported_shas(&v).len(), 5);
}

// ═══════════════════════════════════════════════════════════════════════════
// Flags.
// ═══════════════════════════════════════════════════════════════════════════

/// The flag surface: bad values are errors (exit 1), the new flags parse.
#[test]
fn flag_errors() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    linear_repo(&repo, 1);
    for bad in [
        &["--on-error", "bogus"][..],
        &["--depth", "0"],
        &["--depth", "x"],
        &["--max-commits", "x"],
        &["--examples", "sometimes"],
        &["--max-file-bytes", "99999999999"],
    ] {
        let mut a = vec!["op", "import-git", repo.to_str().unwrap()];
        a.extend_from_slice(bad);
        assert_eq!(env.lex(&a).status.code(), Some(1), "{bad:?}");
    }
    let o = env.lex(&["op", "import-git"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(all_output(&o).contains("usage"), "{}", all_output(&o));
}
