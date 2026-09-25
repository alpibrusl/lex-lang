//! #892 PR 4: `lex op import-git <repo> --head-only` — the first working
//! git → op-log importer (a snapshot of one branch's tip).
//!
//! Every fixture is a REAL git repo built with `git init/commit` under a
//! hermetic environment (no global/system config, pinned identities and
//! dates), and every assertion drives the real `lex` binary.
//!
//! The properties, and the mutation that each one was checked against (the
//! mutation is a one-line change to `import_git.rs` that must turn the named
//! test red — see the PR description):
//!
//! * publish parity: import is equivalent to `lex publish` of a clean checkout
//! * determinism: the `git-import:<root>` session; a twin import and a clone
//!   at another path converge
//! * object-level reading: CRLF / autocrlf / `.gitattributes` cannot leak in
//! * reserved-path filter: a `.lex` file outside src/ is content, inside is not
//! * non-Lex repos: manifest-only, byte-identical `export-git`
//! * provenance round trip: import then `export-git` keeps author/date/message
//! * unsupported kinds, limits, gates, and error messages

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lex_store::Store;
use lex_vcs::{IntentLog, OpLog, OperationRecord};
use tempfile::{tempdir, TempDir};

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── hermetic git ────────────────────────────────────────────────────────────

/// `git` with the developer's config, identity and dates kept out.
fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
    ] {
        c.env_remove(v);
    }
    c.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        // Submodule-free but `protocol.file.allow` matters for `git clone` of a local path.
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

/// `git` with a byte payload on stdin (`hash-object`, `update-index --index-info`).
fn git_stdin(dir: &Path, args: &[&str], input: &[u8]) -> String {
    use std::io::Write;
    let mut child = git_cmd(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const AUTHOR: (&str, &str, &str) = ("Ada Author", "ada@example.org", "2024-03-05T10:20:30+0530");
const COMMITTER: (&str, &str, &str) = ("Carl Committer", "carl@example.org", "2024-03-06T23:59:01-0700");

fn commit_as(dir: &Path, msg: &str, author: (&str, &str, &str), committer: (&str, &str, &str)) -> String {
    let out = git_cmd(dir)
        .args(["commit", "-q", "--allow-empty", "--cleanup=verbatim", "-m", msg])
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_AUTHOR_DATE", author.2)
        .env("GIT_COMMITTER_NAME", committer.0)
        .env("GIT_COMMITTER_EMAIL", committer.1)
        .env("GIT_COMMITTER_DATE", committer.2)
        .output()
        .unwrap();
    assert!(out.status.success(), "git commit: {}", String::from_utf8_lossy(&out.stderr));
    git(dir, &["rev-parse", "HEAD"])
}

fn commit(dir: &Path, msg: &str) -> String {
    commit_as(dir, msg, AUTHOR, COMMITTER)
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

fn chmod_x(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ── the Lex fixture ─────────────────────────────────────────────────────────

const ERROR_LEX: &str = "type Err = { code :: Int, msg :: Str }\n\n\
    fn format(e :: Err) -> Str {\n  e.msg\n}\n";
const LIB_LEX: &str = "import \"./error\" as e\n\n\
    fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n\n\
    fn double(n :: Int) -> Int\n  examples { double(2) => 4 }\n{\n  n * 2\n}\n";
const LIB_LEX_V2: &str = "import \"./error\" as e\n\n\
    fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n\n\
    fn double(n :: Int) -> Int\n  examples { double(2) => 4 }\n{\n  n * 2\n}\n\n\
    fn triple(n :: Int) -> Int { n * 3 }\n";
const LOCK: &str = "# lex.lock\n[[package]]\nname = \"op_v1\"\n";
const BINARY: [u8; 8] = [0x00, 0xff, 0xfe, 0x9f, 0x00, 0x01, 0xc0, 0x80];

/// A two-commit Lex package repo covering every ownership rule: multi-module
/// `src/**/*.lex` (local alias import), a non-`.lex` file nested under `src/`,
/// a `.lex` file OUTSIDE `src/` (content, not op-log), README, an executable,
/// a binary, `.gitignore`, `lex.toml` and `lex.lock`. The tip's author and
/// committer differ and sit in non-UTC zones.
fn lex_repo(dir: &Path) -> (String, String) {
    init(dir);
    write(dir, "lex.toml", b"[package]\nname = \"importpkg\"\nversion = \"0.1.0\"\n");
    write(dir, "lex.lock", LOCK.as_bytes());
    write(dir, "src/error.lex", ERROR_LEX.as_bytes());
    write(dir, "src/lib.lex", LIB_LEX.as_bytes());
    write(dir, "src/data.txt", b"nested non-lex payload under src/\n");
    write(dir, "tools/helper.lex", b"this is a .lex file outside src/: plain content\n");
    write(dir, "README.md", b"# importpkg\n");
    write(dir, "bin/run.sh", b"#!/bin/sh\necho hi\n");
    chmod_x(&dir.join("bin/run.sh"));
    write(dir, "assets/logo.bin", &BINARY);
    write(dir, ".gitignore", b"*.secret\n");
    git(dir, &["add", "-A"]);
    let root = commit(dir, "initial import");
    write(dir, "README.md", b"# importpkg\n\nsecond\n");
    write(dir, "src/lib.lex", LIB_LEX_V2.as_bytes());
    git(dir, &["add", "-A"]);
    let tip = commit(dir, "add triple\n\nSecond paragraph of the message.\n");
    (root, tip)
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
    /// `lex --output json op import-git <repo> --head-only --store <store> <extra..>`.
    fn import(&self, repo: &Path, store: &Path, extra: &[&str]) -> (Output, serde_json::Value) {
        let mut args = vec![
            "--output",
            "json",
            "op",
            "import-git",
            repo.to_str().unwrap(),
            "--head-only",
            "--store",
            store.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let o = self.lex(&args);
        let v = serde_json::from_slice(&o.stdout).unwrap_or(serde_json::Value::Null);
        (o, v)
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

// ── store readers ───────────────────────────────────────────────────────────

fn head(store: &Path, branch: &str) -> Option<String> {
    Store::open(store).unwrap().get_branch(branch).unwrap().and_then(|b| b.head_op)
}

fn records(store: &Path, branch: &str) -> Vec<OperationRecord> {
    let h = head(store, branch).expect("branch has a head");
    OpLog::open(store).unwrap().walk_forward(&h, None).unwrap()
}

/// The op kinds of a branch, oldest first (sig/stage/manifest ids included,
/// intent excluded — the intent is what differs between an import and a publish).
fn kinds(store: &Path, branch: &str) -> Vec<String> {
    records(store, branch).iter().map(|r| serde_json::to_string(&r.op.kind).unwrap()).collect()
}

/// `path -> (mode, bytes)` of the files manifest in force at the branch head.
fn manifest_files(store: &Path, branch: &str) -> BTreeMap<String, (String, Vec<u8>)> {
    let s = Store::open(store).unwrap();
    let at = s.branch_manifest(branch).unwrap();
    let Some(id) = at.manifest() else { return BTreeMap::new() };
    let m = s.get_manifest(id).unwrap();
    m.entries
        .iter()
        .map(|(p, e)| (p.clone(), (e.mode.clone(), s.get_blob_bytes(&e.blob).unwrap())))
        .collect()
}

fn manifest_id(store: &Path, branch: &str) -> Option<String> {
    Store::open(store).unwrap().branch_manifest(branch).unwrap().manifest().cloned()
}

fn branches(store: &Path) -> Vec<String> {
    let s = Store::open(store).unwrap();
    let mut b = s.list_branches().unwrap();
    b.retain(|n| s.get_branch(n).unwrap().is_some());
    b
}

// ═══════════════════════════════════════════════════════════════════════════
// Publish parity — the key property.
// ═══════════════════════════════════════════════════════════════════════════

/// Import a repo's tip, and separately `lex publish` a CLEAN CHECKOUT of the
/// same ref into another store: the same sig→stage map, the same op kinds, the
/// same manifest id and the same committed lock. (The OpIds themselves differ —
/// the two commands stamp different intents, and the intent is hashed into every
/// op — so equality is asserted on everything the intent does not touch.)
///
/// Mutation: dropping the `src/**/*.lex` / `src.lex` filter of the manifest
/// makes `tools/helper.lex`'s neighbour `src/error.lex` reach
/// `manifest_from_files`, which rejects reserved paths → the tip stops landing.
#[test]
fn import_matches_publish_of_a_clean_checkout() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let (_root, tip) = lex_repo(&repo);

    let imp = t.path().join("store-import");
    let (o, v) = env.import(&repo, &imp, &[]);
    assert!(o.status.success(), "import failed: {} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["landed"], true);
    assert_eq!(data(&v)["tip"]["sha"], tip.as_str());

    // A clean checkout of the same ref, published the ordinary way.
    let checkout = t.path().join("checkout");
    git(t.path(), &["clone", "-q", repo.to_str().unwrap(), checkout.to_str().unwrap()]);
    let publ = t.path().join("store-publish");
    env.ok(&[
        "publish",
        "--store",
        publ.to_str().unwrap(),
        "--branch",
        "main",
        "--intent-prompt",
        "parity",
        "--intent-session",
        "parity-session",
        checkout.to_str().unwrap(),
    ]);

    let (si, sp) = (Store::open(&imp).unwrap(), Store::open(&publ).unwrap());
    let map_i = si.branch_head("main").unwrap();
    let map_p = sp.branch_head("main").unwrap();
    assert_eq!(map_i.len(), 5, "double, triple, render, format + the Err type: {map_i:?}");
    assert_eq!(map_i, map_p, "same sig -> stage map");
    assert_eq!(kinds(&imp, "main"), kinds(&publ, "main"), "same op kinds in the same order");
    assert_eq!(manifest_id(&imp, "main"), manifest_id(&publ, "main"), "same manifest id");
    assert!(manifest_id(&imp, "main").is_some());
    assert_eq!(
        si.committed_lock(&head(&imp, "main").unwrap()).unwrap(),
        sp.committed_lock(&head(&publ, "main").unwrap()).unwrap(),
        "the committed lock rides on the final head, as `lex publish` leaves it"
    );
    assert_eq!(
        si.committed_lock(&head(&imp, "main").unwrap()).unwrap().as_deref(),
        Some(LOCK)
    );

    // Not vacuous: the OpIds do differ (the intents differ).
    assert_ne!(head(&imp, "main"), head(&publ, "main"));

    // The manifest holds every non-op-log file — including the `.lex` file
    // that is OUTSIDE src/ and the non-lex file nested UNDER src/ — and none
    // of `src/**/*.lex`; modes survive.
    let files = manifest_files(&imp, "main");
    let paths: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        vec![
            ".gitignore",
            "README.md",
            "assets/logo.bin",
            "bin/run.sh",
            "lex.lock",
            "lex.toml",
            "src/data.txt",
            "tools/helper.lex"
        ]
    );
    assert_eq!(files["bin/run.sh"].0, "100755");
    assert_eq!(files["README.md"].0, "100644");
    assert_eq!(files["assets/logo.bin"].1, BINARY);
    assert_eq!(files["tools/helper.lex"].1, b"this is a .lex file outside src/: plain content\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// Determinism.
// ═══════════════════════════════════════════════════════════════════════════

/// Two independent imports of the same repo into two fresh stores give the same
/// head OpId, and so does an import from a `git clone` at a different path:
/// repo identity is the root commit, not the path.
///
/// Mutation: replacing the `git-import:<root>` session with the default
/// (`cli-<pid>-<epoch>`, i.e. any per-process value) makes the twin imports
/// differ.
#[test]
fn twin_imports_and_a_clone_at_another_path_converge() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let (root, tip) = lex_repo(&repo);

    let (a, b) = (t.path().join("store-a"), t.path().join("store-b"));
    assert!(env.import(&repo, &a, &[]).0.status.success());
    // A second process, later in time, another store, another store-branch name.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(env.import(&repo, &b, &["--store-branch", "renamed"]).0.status.success());
    let (ha, hb) = (head(&a, "main").unwrap(), head(&b, "renamed").unwrap());
    assert_eq!(ha, hb, "identical repo => identical head OpId");

    // A clone at a different path (the clone's own checkout differs, its objects don't).
    let clone = t.path().join("elsewhere").join("clone");
    std::fs::create_dir_all(clone.parent().unwrap()).unwrap();
    git(t.path(), &["clone", "-q", repo.to_str().unwrap(), clone.to_str().unwrap()]);
    let c = t.path().join("store-c");
    assert!(env.import(&clone, &c, &[]).0.status.success());
    assert_eq!(head(&c, "main").unwrap(), ha, "repo identity is the root sha, not the path");

    // What was hashed: the deterministic intent, on every op.
    let intents = IntentLog::open(&a).unwrap();
    let recs = records(&a, "main");
    assert!(recs.len() >= 5, "semantic ops + SetFiles: {}", recs.len());
    let ids: std::collections::BTreeSet<_> = recs.iter().map(|r| r.op.intent_id.clone()).collect();
    assert_eq!(ids.len(), 1, "one commit, one intent, shared by every op");
    let intent = intents.get(ids.iter().next().unwrap().as_ref().unwrap()).unwrap().unwrap();
    assert_eq!(intent.session_id, format!("git-import:{root}"));
    assert_eq!((intent.model.provider.as_str(), intent.model.name.as_str()), ("git", "import"));
    assert_eq!(intent.model.version.as_deref(), Some("1"));
    assert_eq!(intent.prompt, "add triple\n\nSecond paragraph of the message.\n");
    let o = intent.origin.expect("origin");
    assert_eq!((o.vcs.as_str(), o.commit.as_str()), ("git", tip.as_str()));
    assert_eq!((o.author.name.as_str(), o.author.email.as_str()), ("Ada Author", "ada@example.org"));
    assert_eq!((o.author.when, o.author.tz.as_str()), (1_709_614_230, "+0530"));
    let c = o.committer.expect("committer");
    assert_eq!((c.name.as_str(), c.email.as_str()), ("Carl Committer", "carl@example.org"));
    assert_eq!(c.tz, "-0700");
    assert_eq!(o.parents, vec![root.clone()]);
    assert!(o.folded.is_empty());
    assert_eq!(intent.created_at, c.when as u64, "created_at is the committer date");

    // Negative control: a different commit message is a different intent, so
    // different OpIds — the equality above is a real property.
    let repo2 = t.path().join("repo2");
    lex_repo(&repo2);
    git(&repo2, &["reset", "-q", "--soft", "HEAD~1"]);
    commit(&repo2, "another message");
    let d = t.path().join("store-d");
    assert!(env.import(&repo2, &d, &[]).0.status.success());
    assert_ne!(head(&d, "main").unwrap(), ha);
}

// ═══════════════════════════════════════════════════════════════════════════
// Object-level reading.
// ═══════════════════════════════════════════════════════════════════════════

/// The importer reads git OBJECTS, never a checkout. The repo has
/// `core.autocrlf=true` and a `.gitattributes` `text eol=crlf` rule, and its
/// worktree really is CRLF-converted, but the manifest holds the exact object
/// bytes (an LF blob stays LF; a blob stored with CRLF stays CRLF) and the
/// import converges with a clone that has no such config.
///
/// Mutation: building the manifest from the worktree file instead of the
/// object turns `lf.txt` into CRLF and this test red.
#[test]
fn bytes_come_from_objects_not_a_checkout() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    git(&repo, &["config", "core.autocrlf", "true"]);
    write(&repo, ".gitattributes", b"*.txt text eol=crlf\n");
    write(&repo, "lex.toml", b"[package]\nname = \"eolpkg\"\nversion = \"0.1.0\"\n");
    write(&repo, "src/main.lex", b"fn one() -> Int { 1 }\n");
    git(&repo, &["add", "-A"]);
    // Objects written WITHOUT filters, so the stored bytes are exactly these:
    // an LF blob under an `eol=crlf` rule, and a blob that really contains CRLF.
    let lf = git_stdin(&repo, &["hash-object", "-w", "--no-filters", "--stdin"], b"unix\nlines\n");
    let crlf = git_stdin(&repo, &["hash-object", "-w", "--no-filters", "--stdin"], b"dos\r\nlines\r\n");
    git_stdin(
        &repo,
        &["update-index", "--index-info"],
        format!("100644 {lf}\tlf.txt\n100644 {crlf}\tcrlf.txt\n").as_bytes(),
    );
    commit(&repo, "eol fixture");
    // Materialize the checkout WITH the conversions applied: the worktree
    // bytes now differ from the objects.
    std::fs::remove_file(repo.join("lf.txt")).ok();
    git(&repo, &["checkout", "-q", "--", "lf.txt", "crlf.txt"]);
    assert_eq!(
        std::fs::read(repo.join("lf.txt")).unwrap(),
        b"unix\r\nlines\r\n",
        "precondition: the checkout converted the LF object to CRLF"
    );

    let a = t.path().join("store-a");
    let (o, _) = env.import(&repo, &a, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    let files = manifest_files(&a, "main");
    assert_eq!(files["lf.txt"].1, b"unix\nlines\n", "the object's bytes, not the checkout's");
    assert_eq!(files["crlf.txt"].1, b"dos\r\nlines\r\n", "CRLF preserved verbatim");

    // A clone with no autocrlf shares the objects; it must import to the same OpIds.
    let clone = t.path().join("clone");
    git(t.path(), &["clone", "-q", "-c", "core.autocrlf=false", repo.to_str().unwrap(), clone.to_str().unwrap()]);
    let b = t.path().join("store-b");
    assert!(env.import(&clone, &b, &[]).0.status.success());
    assert_eq!(head(&a, "main"), head(&b, "main"), "machine-dependent bytes would break convergence");
}

// ═══════════════════════════════════════════════════════════════════════════
// Non-Lex repos and the export round trip.
// ═══════════════════════════════════════════════════════════════════════════

/// A repo with no `lex.toml` and no `src/**/*.lex` imports as a manifest-only
/// branch (zero semantic ops, exactly one `SetFiles`), and `export-git`
/// reproduces the tree byte-for-byte (contents and exec bits), stamping the
/// ORIGINAL author, committer, dates and message plus a `Git-Source` trailer.
#[test]
fn non_lex_repo_is_manifest_only_and_exports_byte_identically() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "README.md", b"# just docs\r\nwith CRLF\r\n");
    write(&repo, "docs/guide.md", b"guide\n");
    write(&repo, "scripts/run.sh", b"#!/bin/sh\nexit 0\n");
    chmod_x(&repo.join("scripts/run.sh"));
    write(&repo, "img/logo.bin", &BINARY);
    write(&repo, ".gitignore", b"target/\n");
    write(&repo, "notes.lex", b"a stray .lex file at the root is content\n");
    git(&repo, &["add", "-A"]);
    // A message that itself ends in trailer-looking lines.
    let msg = "docs only\n\nSigned-off-by: Someone <s@example.org>\nOp: fake\n";
    let tip = commit(&repo, msg);

    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    let imported = &data(&v)["imported"];
    assert_eq!(imported[0]["ops"], 1, "one SetFiles, nothing else");
    assert!(imported[0]["files_op"].is_string());

    let recs = records(&store, "main");
    assert_eq!(recs.len(), 1);
    assert!(matches!(recs[0].op.kind, lex_vcs::OperationKind::SetFiles { .. }));
    assert!(Store::open(&store).unwrap().branch_head("main").unwrap().is_empty(), "zero semantic ops");

    // Round trip through export-git.
    let out = t.path().join("exported");
    env.ok(&["export-git", out.to_str().unwrap(), "--store", store.to_str().unwrap(), "--branch", "main"]);
    let (src_files, out_files) = (tracked(&repo), tracked(&out));
    assert_eq!(src_files, out_files, "same file set");
    for f in &src_files {
        assert_eq!(std::fs::read(repo.join(f)).unwrap(), std::fs::read(out.join(f)).unwrap(), "{f} byte-identical");
        assert_eq!(is_exec(&repo.join(f)), is_exec(&out.join(f)), "{f} mode");
    }

    // Provenance (PR 3's export, fed by PR 4's origin).
    let fmt = "%an|%ae|%aI|%cn|%ce|%cI";
    assert_eq!(git(&out, &["log", "-1", &format!("--format={fmt}")]), git(&repo, &["log", "-1", &format!("--format={fmt}")]));
    let body = git(&out, &["log", "-1", "--format=%B"]);
    assert!(body.starts_with(msg.trim_end()), "message verbatim first: {body:?}");
    assert!(body.contains(&format!("Git-Source: {tip}")), "{body:?}");
    assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]), "1", "one commit for one imported commit");
}

fn tracked(dir: &Path) -> Vec<String> {
    git(dir, &["ls-files"]).lines().map(str::to_string).collect()
}

fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
}

/// A Lex package round-trips its provenance too, and a MERGE tip records every
/// parent (first parent first) in the origin.
#[test]
fn lex_import_round_trips_provenance_through_export_git() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let (root, _) = lex_repo(&repo);
    // A side branch and a --no-ff merge as the tip.
    git(&repo, &["checkout", "-q", "-b", "side", &root]);
    write(&repo, "SIDE.md", b"side\n");
    git(&repo, &["add", "-A"]);
    let side = commit(&repo, "side work");
    git(&repo, &["checkout", "-q", "main"]);
    let first_parent = git(&repo, &["rev-parse", "HEAD"]);
    let out = git_cmd(&repo)
        .args(["merge", "-q", "--no-ff", "-m", "merge side", "side"])
        .env("GIT_AUTHOR_NAME", "Merger")
        .env("GIT_AUTHOR_EMAIL", "m@example.org")
        .env("GIT_AUTHOR_DATE", "2024-04-01T00:00:00+0200")
        .env("GIT_COMMITTER_NAME", "Merger")
        .env("GIT_COMMITTER_EMAIL", "m@example.org")
        .env("GIT_COMMITTER_DATE", "2024-04-01T00:00:00+0200")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let store = t.path().join("store");
    let (o, _) = env.import(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    // The merge's tree diff (vs first parent) folds the side work in: SIDE.md is present.
    assert!(manifest_files(&store, "main").contains_key("SIDE.md"));
    let intent_id = records(&store, "main")[0].op.intent_id.clone().unwrap();
    let origin = IntentLog::open(&store).unwrap().get(&intent_id).unwrap().unwrap().origin.unwrap();
    assert_eq!(origin.commit, tip);
    assert_eq!(origin.parents, vec![first_parent, side], "all parents, first first");

    let exported = t.path().join("exported");
    env.ok(&["export-git", exported.to_str().unwrap(), "--store", store.to_str().unwrap()]);
    let fmt = "--format=%an|%ae|%aI|%cn|%ce|%cI";
    assert_eq!(git(&exported, &["log", "-1", fmt]), git(&repo, &["log", "-1", fmt]));
    let body = git(&exported, &["log", "-1", "--format=%B"]);
    assert!(body.starts_with("merge side"), "{body:?}");
    assert!(body.contains(&format!("Git-Source: {tip}")), "{body:?}");
    // Every non-`.lex` file the repo tracks is byte-identical in the export.
    for f in tracked(&repo).into_iter().filter(|f| !(f.starts_with("src/") && f.ends_with(".lex"))) {
        assert_eq!(std::fs::read(repo.join(&f)).unwrap(), std::fs::read(exported.join(&f)).unwrap(), "{f}");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Unsupported kinds, limits.
// ═══════════════════════════════════════════════════════════════════════════

fn symlink_and_submodule_repo(dir: &Path) -> String {
    init(dir);
    write(dir, "README.md", b"hello\n");
    write(dir, "real.txt", b"real\n");
    std::os::unix::fs::symlink("real.txt", dir.join("link.txt")).unwrap();
    git(dir, &["add", "-A"]);
    // A gitlink (submodule) entry, without needing a real submodule.
    git(
        dir,
        &["update-index", "--add", "--cacheinfo", "160000,1234567890123456789012345678901234567890,vendor/sub"],
    );
    commit(dir, "symlink and submodule")
}

/// Symlinks and submodules are skipped, listed in the report as
/// `unsupported:[{path,kind,commit}]`, and the rest of the tree still imports;
/// `--strict` turns them into exit 2 and nothing lands.
#[test]
fn symlinks_and_submodules_are_skipped_and_listed_and_strict_fails() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let tip = symlink_and_submodule_repo(&repo);

    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    let mut un: Vec<(String, String, String)> = data(&v)["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (u["path"].as_str().unwrap().into(), u["kind"].as_str().unwrap().into(), u["commit"].as_str().unwrap().into())
        })
        .collect();
    un.sort();
    assert_eq!(
        un,
        vec![
            ("link.txt".to_string(), "symlink".to_string(), tip.clone()),
            ("vendor/sub".to_string(), "submodule".to_string(), tip.clone()),
        ]
    );
    let paths: Vec<String> = manifest_files(&store, "main").keys().cloned().collect();
    assert_eq!(paths, vec!["README.md", "real.txt"], "the tree still imports, minus the unsupported paths");

    // --strict: exit 2, nothing lands.
    let strict_store = t.path().join("store-strict");
    let (o, v) = env.import(&repo, &strict_store, &["--strict"]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["landed"], false);
    assert_eq!(data(&v)["tip"]["phase"], "strict");
    assert_eq!(data(&v)["unsupported"].as_array().unwrap().len(), 2);
    assert_eq!(head(&strict_store, "main"), None, "--strict lands nothing");
}

/// A file over the 8 MiB manifest limit refuses the commit with
/// `manifest:limit` (checked from the tree's object size, before any byte is
/// read) and nothing lands. So do >10k entries and a lowered
/// `--max-file-bytes`.
#[test]
fn oversized_files_and_too_many_entries_are_refused_locally() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "small.txt", b"ok\n");
    write(&repo, "big.bin", &vec![0x5au8; 8 * 1024 * 1024 + 1]);
    git(&repo, &["add", "-A"]);
    commit(&repo, "big file");

    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    let tip = &data(&v)["tip"];
    assert_eq!(tip["landed"], false);
    assert_eq!(tip["reason"], "manifest:limit");
    assert!(tip["message"].as_str().unwrap().contains("big.bin"), "{tip}");
    assert_eq!(head(&store, "main"), None);
    assert_eq!(branches(&store), Vec::<String>::new(), "no work branch left behind");

    // A lowered limit applies to a file that would otherwise pass.
    let repo_b = t.path().join("repo-b");
    init(&repo_b);
    write(&repo_b, "a.txt", b"0123456789");
    git(&repo_b, &["add", "-A"]);
    commit(&repo_b, "ten bytes");
    let s2 = t.path().join("store2");
    let (o, v) = env.import(&repo_b, &s2, &["--max-file-bytes", "9"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(data(&v)["tip"]["reason"], "manifest:limit");
    assert!(env.import(&repo_b, &s2, &["--max-file-bytes", "10"]).0.status.success());

    // >10k entries: one shared blob under 10_001 paths.
    let repo_c = t.path().join("repo-c");
    init(&repo_c);
    let blob = git_stdin(&repo_c, &["hash-object", "-w", "--stdin"], b"x\n");
    let idx: String = (0..10_001).map(|i| format!("100644 {blob}\tf/{i:05}.txt\n")).collect();
    git_stdin(&repo_c, &["update-index", "--add", "--index-info"], idx.as_bytes());
    commit(&repo_c, "many files");
    let s3 = t.path().join("store3");
    let (o, v) = env.import(&repo_c, &s3, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["reason"], "manifest:limit");
    assert_eq!(head(&s3, "main"), None);
}

/// Paths that differ only in case cannot coexist on every filesystem, so the
/// commit is refused with `manifest:case_collision` — including a collision
/// between two `.lex` sources, which never reach the manifest.
#[test]
fn case_only_path_collisions_are_refused() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    let blob = git_stdin(&repo, &["hash-object", "-w", "--stdin"], b"x\n");
    git_stdin(
        &repo,
        &["update-index", "--add", "--index-info"],
        format!("100644 {blob}\tREADME\n100644 {blob}\treadme\n").as_bytes(),
    );
    commit(&repo, "collision");
    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["reason"], "manifest:case_collision");
    assert_eq!(head(&store, "main"), None);

    let repo2 = t.path().join("repo2");
    init(&repo2);
    let lex = git_stdin(&repo2, &["hash-object", "-w", "--stdin"], b"fn a() -> Int { 1 }\n");
    let toml = git_stdin(
        &repo2,
        &["hash-object", "-w", "--stdin"],
        b"[package]\nname = \"c\"\nversion = \"0.1.0\"\n",
    );
    git_stdin(
        &repo2,
        &["update-index", "--add", "--index-info"],
        format!("100644 {toml}\tlex.toml\n100644 {lex}\tsrc/a.lex\n100644 {lex}\tsrc/A.lex\n").as_bytes(),
    );
    commit(&repo2, "lex collision");
    let s2 = t.path().join("store2");
    let (o, v) = env.import(&repo2, &s2, &[]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(data(&v)["tip"]["reason"], "manifest:case_collision");
}

// ═══════════════════════════════════════════════════════════════════════════
// Gates and atomicity.
// ═══════════════════════════════════════════════════════════════════════════

/// A tip that does not type-check lands NOTHING (the branch stays absent — no
/// half-applied commit, no stray work branch), exits 2, and the report says
/// `tip:{sha, landed:false, phase, diagnostics}`.
#[test]
fn a_tip_that_does_not_type_check_lands_nothing() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "lex.toml", b"[package]\nname = \"badpkg\"\nversion = \"0.1.0\"\n");
    write(&repo, "src/main.lex", b"fn good() -> Int { 1 }\nfn bad() -> Int { \"not an int\" }\n");
    write(&repo, "README.md", b"# bad\n");
    git(&repo, &["add", "-A"]);
    let tip = commit(&repo, "does not type-check");

    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    let tip_json = &data(&v)["tip"];
    assert_eq!(tip_json["sha"], tip.as_str());
    assert_eq!(tip_json["landed"], false);
    assert_eq!(tip_json["phase"], "type-check");
    assert!(!tip_json["diagnostics"].as_array().unwrap().is_empty(), "{tip_json}");
    assert_eq!(data(&v)["imported"].as_array().unwrap().len(), 0);
    assert_eq!(head(&store, "main"), None, "the store branch is untouched");
    assert_eq!(branches(&store), Vec::<String>::new(), "and no work branch is left behind");
    assert!(
        Store::open(&store).unwrap().branch_manifest("main").is_err()
            || manifest_id(&store, "main").is_none(),
        "no SetFiles either"
    );

    // Text mode: diagnostics on stderr, exit 2.
    let out = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--store", store.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).contains("did NOT land"), "{}", stdout(&out));
    assert!(!stderr(&out).is_empty(), "diagnostics are printed");
}

/// `--examples tip` (the default) runs the examples gate as `lex publish`
/// does; `--examples none` skips it.
#[test]
fn the_examples_gate_runs_at_the_tip_unless_disabled() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "lex.toml", b"[package]\nname = \"expkg\"\nversion = \"0.1.0\"\n");
    write(&repo, "src/main.lex", b"fn double(n :: Int) -> Int\n  examples { double(2) => 5 }\n{\n  n * 2\n}\n");
    git(&repo, &["add", "-A"]);
    commit(&repo, "wrong example");

    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["phase"], "examples");
    assert_eq!(head(&store, "main"), None);

    let (o, _) = env.import(&repo, &store, &["--examples", "none"]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert!(head(&store, "main").is_some());
}

// ═══════════════════════════════════════════════════════════════════════════
// Errors.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn src_lex_without_lex_toml_is_refused() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    init(&repo);
    write(&repo, "src/main.lex", b"fn f() -> Int { 1 }\n");
    write(&repo, "README.md", b"# no manifest\n");
    git(&repo, &["add", "-A"]);
    commit(&repo, "sources without a manifest");
    let store = t.path().join("store");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["reason"], "lex:no_manifest");
    assert!(data(&v)["tip"]["message"].as_str().unwrap().contains("lex.toml"));
    assert_eq!(head(&store, "main"), None);

    // A lex.toml with no [package] name is equally not a package.
    write(&repo, "lex.toml", b"[dependencies]\n");
    git(&repo, &["add", "-A"]);
    commit(&repo, "manifest without a package name");
    let (o, v) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(data(&v)["tip"]["reason"], "lex:not_a_package");
}

/// A non-empty target branch is refused (incremental import is PR 5), with a
/// hint at `--store-branch`; a fresh `--store-branch` works.
#[test]
fn a_non_empty_target_branch_is_refused() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    lex_repo(&repo);
    let store = t.path().join("store");
    assert!(env.import(&repo, &store, &[]).0.status.success());
    let before = head(&store, "main");

    let (o, _) = env.import(&repo, &store, &[]);
    assert_eq!(o.status.code(), Some(1));
    let (out, err) = (stdout(&o), stderr(&o));
    assert!(out.contains("--store-branch") || err.contains("--store-branch"), "{out} {err}");
    assert_eq!(head(&store, "main"), before, "the branch is untouched");
    assert_eq!(branches(&store), vec!["main".to_string()], "no work branch left behind");

    assert!(env.import(&repo, &store, &["--store-branch", "second"]).0.status.success());
    assert_eq!(head(&store, "second"), before, "same repo, same OpIds, another branch");
}

#[test]
fn flag_surface_and_history_default() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    lex_repo(&repo);
    let store = t.path().join("store");

    // PR 5: without --head-only the whole first-parent history is imported
    // (tests/import_git_history_892.rs covers it in depth; URL sources too).
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--store", store.to_str().unwrap()]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert!(head(&store, "main").is_some());

    // PR 5's flags parse and validate.
    assert!(env
        .lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--on-error", "stop", "--examples", "all", "--store", t.path().join("s2").to_str().unwrap()])
        .status
        .success());
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--on-error", "bogus"]);
    assert_eq!(o.status.code(), Some(1));
    let o = env.lex(&["op", "import-git", repo.to_str().unwrap(), "--head-only", "--max-file-bytes", "99999999999"]);
    assert_eq!(o.status.code(), Some(1));
}

/// `--branch` picks a non-HEAD branch; a missing one is an error; a branch
/// name a store cannot hold needs `--store-branch`; a shallow clone is refused
/// (its root commit — the repo's identity — is not the real root).
#[test]
fn branch_selection_and_repo_shape_errors() {
    let t = tempdir().unwrap();
    let env = Env::new();
    let repo = t.path().join("repo");
    let (root, _) = lex_repo(&repo);
    git(&repo, &["branch", "old", &root]);
    git(&repo, &["branch", "feature/x", &root]);

    let s = t.path().join("s");
    let (o, v) = env.import(&repo, &s, &["--branch", "old"]);
    assert!(o.status.success(), "{} {}", stdout(&o), stderr(&o));
    assert_eq!(data(&v)["tip"]["sha"], root.as_str());
    assert!(head(&s, "old").is_some(), "store branch defaults to the git branch's name");
    assert_eq!(Store::open(&s).unwrap().branch_head("old").unwrap().len(), 4);

    let (o, _) = env.import(&repo, &s, &["--branch", "nope"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("no branch") || stdout(&o).contains("no branch"));

    let (o, _) = env.import(&repo, &s, &["--branch", "feature/x"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(format!("{}{}", stdout(&o), stderr(&o)).contains("--store-branch"));
    assert!(env.import(&repo, &s, &["--branch", "feature/x", "--store-branch", "feature-x"]).0.status.success());

    let shallow = t.path().join("shallow");
    git(t.path(), &["clone", "-q", "--depth", "1", &format!("file://{}", repo.display()), shallow.to_str().unwrap()]);
    let (o, _) = env.import(&shallow, &t.path().join("s2"), &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(format!("{}{}", stdout(&o), stderr(&o)).contains("shallow"));

    let plain = t.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let (o, _) = env.import(&plain, &t.path().join("s3"), &[]);
    assert_eq!(o.status.code(), Some(1));
}
