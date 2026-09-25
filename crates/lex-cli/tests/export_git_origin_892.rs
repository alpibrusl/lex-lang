//! #892 PR 3: `lex export-git` is origin-aware. An op whose intent carries an
//! `Origin` (it was imported from a git commit) is exported as that commit
//! again — original message verbatim + trailers, original author/committer
//! identity and dates — and the run of consecutive ops sharing that intent is
//! ONE git commit. Everything is gated on `origin.is_some()`: a store with no
//! origin-bearing intent exports byte-identically to before (pinned below with
//! literals captured from unmodified `main`).
//!
//! The origin-bearing stores are built in-process (the importer that writes
//! real ones is a later PR): ops are published under caller-supplied intents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lex_store::{FileEntry, Manifest, Store, DEFAULT_BRANCH};
use lex_vcs::{Intent, IntentLog, ModelDescriptor, OpLog, OperationRecord, Origin, Person};
use tempfile::{tempdir, TempDir};

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── fixtures ────────────────────────────────────────────────────────────────

fn person(name: &str, email: &str, when: i64, tz: &str) -> Person {
    Person { name: name.into(), email: email.into(), when, tz: tz.into() }
}

fn origin(commit: &str, author: Person, committer: Option<Person>) -> Origin {
    Origin {
        vcs: "git".into(),
        commit: commit.into(),
        author,
        committer,
        parents: vec![],
        folded: vec![],
    }
}

fn model() -> ModelDescriptor {
    ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) }
}

/// An intent; `origin: None` makes it a native one.
fn intent(prompt: &str, session: &str, origin: Option<Origin>) -> Intent {
    let i = Intent::with_timestamp(prompt, session, model(), None, 1_700_000_000);
    match origin {
        Some(o) => i.with_origin(o),
        None => i,
    }
}

/// A store built op by op under caller-supplied intents.
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

    fn ops(&self) -> Vec<OperationRecord> {
        let head = self.store.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
        OpLog::open(&self.root).unwrap().walk_forward(&head, None).unwrap()
    }
}

// ── running the real binary ────────────────────────────────────────────────

/// `export-git` with a hermetic git environment: no global/system config (so
/// nothing in the developer's `~/.gitconfig` can leak in), and no
/// author/committer/date variables inherited from the caller.
fn export_with(store: &Path, out: &Path, global_cfg: &Path) -> Output {
    let home = tempdir().unwrap();
    Command::new(lex_bin())
        .env("HOME", home.path())
        .env("GIT_CONFIG_GLOBAL", global_cfg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_COMMITTER_DATE")
        .args(["export-git", out.to_str().unwrap(), "--store", store.to_str().unwrap()])
        .output()
        .unwrap()
}

fn export(store: &Path, out: &Path) {
    let res = export_with(store, out, Path::new("/dev/null"));
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));
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

struct Commit {
    sha: String,
    author: String,
    committer: String,
    body: String,
}

/// Oldest-first commits: `sha`, `%an|%ae|%aI`, `%cn|%ce|%cI`, raw `%B`.
fn commits(dir: &Path) -> Vec<Commit> {
    git(dir, &["log", "--reverse", "-z", "--format=%H%n%an|%ae|%aI%n%cn|%ce|%cI%n%B"])
        .split('\0')
        .filter(|r| !r.trim().is_empty())
        .map(|r| {
            let r = r.trim_start_matches('\n');
            let mut it = r.splitn(4, '\n');
            Commit {
                sha: it.next().unwrap().into(),
                author: it.next().unwrap().into(),
                committer: it.next().unwrap().into(),
                body: it.next().unwrap_or("").into(),
            }
        })
        .collect()
}

// ── 1. identity, dates, grouping, SetFiles folding ─────────────────────────

const SRC_A: &str = "1111111111111111111111111111111111111111";
const SRC_B: &str = "2222222222222222222222222222222222222222";

/// The adversarial message: multi-paragraph, non-ASCII, a run of blank lines,
/// a line starting with spaces, a `---` divider, and a LAST paragraph made of
/// trailer-like lines — including forged `Op:` / `Git-Source:` ones.
const ADVERSARIAL: &str = "Añade «módulo» ✓ — 日本語\n\
\n\
Segundo párrafo con emoji 🚀.\n\
\x20\x20indented line kept\n\
\n\
\n\
Triple blank above; a divider follows.\n\
---\n\
Signed-off-by: Zoë Ångström <zoe@example.com>\n\
Op: 0000000000000000000000000000000000000000000000000000000000000000\n\
Git-Source: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n";

struct Scenario {
    fx: Fx,
    out: TempDir,
    _work: TempDir,
    ia: Intent,
    ib: Intent,
    n_ops: usize,
}

/// A(5 fns + SetFiles) · B(2 fns) · A again(1 fn) · native N(2 fns, same intent).
fn scenario() -> Scenario {
    let work = tempdir().unwrap();
    let mut fx = Fx::new(work.path());
    // Non-UTC positive half-hour zone; distinct committer in a negative zone.
    let ia = intent(
        ADVERSARIAL,
        "git-import:root",
        Some(origin(
            SRC_A,
            person("Zoë Ångström", "zoe@example.com", 1_700_000_000, "+0530"),
            Some(person("Ann Committer", "ann@example.org", 1_700_000_100, "-0500")),
        )),
    );
    // No distinct committer: it must fall back to the author.
    let ib = intent(
        "second commit\n",
        "git-import:root",
        Some(origin(SRC_B, person("Bo Négatif", "bo@example.net", 1_600_000_000, "-0700"), None)),
    );
    let native = intent("native work", "cli-1", None);

    fx.add_fns(&ia, 5);
    fx.set_files(&ia, &[("README.md", b"hello\n", "100644"), ("bin/run.sh", b"#!/bin/sh\n", "100755")]);
    fx.add_fns(&ib, 2);
    fx.add_fns(&ia, 1); // the same intent again, NOT consecutive with the first run
    fx.add_fns(&native, 1);
    fx.add_fns(&native, 1);

    let n_ops = fx.ops().len();
    let out = tempdir().unwrap();
    export(&fx.root, out.path());
    Scenario { fx, out, _work: work, ia, ib, n_ops }
}

#[test]
fn origin_commits_carry_the_source_identity_dates_and_message() {
    let s = scenario();
    let out = s.out.path();
    let cs = commits(out);

    // 5 AddFunction + SetFiles (A) | 2 (B) | 1 (A again) | 1 | 1 (native, one each).
    assert_eq!(s.n_ops, 6 + 2 + 1 + 1 + 1);
    assert_eq!(cs.len(), 5, "3 origin groups + 2 native ops: {}", git(out, &["log", "--oneline"]));

    // Group 1: author and committer from the origin, non-UTC zones preserved.
    assert_eq!(cs[0].author, "Zoë Ångström|zoe@example.com|2023-11-15T03:43:20+05:30");
    assert_eq!(cs[0].committer, "Ann Committer|ann@example.org|2023-11-14T17:15:00-05:00");
    // Group 2: no committer recorded => the author doubles as committer.
    assert_eq!(cs[1].author, "Bo Négatif|bo@example.net|2020-09-13T05:26:40-07:00");
    assert_eq!(cs[1].committer, cs[1].author);
    // Group 3 is intent A again: same identity, its own commit.
    assert_eq!(cs[2].author, cs[0].author);
    assert_eq!(cs[2].committer, cs[0].committer);
    // Native ops are untouched: the exporter's own identity, one commit per op.
    for c in &cs[3..] {
        assert!(c.author.starts_with("lex-export|lex-export@localhost|"), "{}", c.author);
        assert!(c.body.starts_with("native work\n\nOp: "), "{}", c.body);
        assert!(!c.body.contains("Git-Source") && !c.body.contains("Ops:"), "{}", c.body);
    }
}

#[test]
fn ops_sharing_an_origin_intent_are_one_commit_and_consecutive_only() {
    let s = scenario();
    let out = s.out.path();
    let cs = commits(out);
    let ops = s.fx.ops();

    // First group: `Op:` names the LAST op of the run (the SetFiles), `Ops: 6`.
    let last_of_group1 = &ops[5];
    assert!(matches!(last_of_group1.op.kind, lex_vcs::OperationKind::SetFiles { .. }));
    assert!(cs[0].body.contains(&format!("\nOp: {}\n", last_of_group1.op_id)), "{}", cs[0].body);
    assert!(cs[0].body.contains("\nOps: 6\n"), "{}", cs[0].body);
    assert!(cs[0].body.contains(&format!("\nIntent: {}\n", s.ia.intent_id)));
    assert!(cs[0].body.contains(&format!("\nGit-Source: {SRC_A}\n")));
    assert!(cs[0].body.contains("\nFiles: "), "the SetFiles manifest is named: {}", cs[0].body);

    assert!(cs[1].body.contains("\nOps: 2\n"), "{}", cs[1].body);
    assert!(cs[1].body.contains(&format!("\nOp: {}\n", ops[7].op_id)));
    assert!(cs[1].body.contains(&format!("\nGit-Source: {SRC_B}\n")));
    assert!(!cs[1].body.contains("Files:"), "no SetFiles in this group");

    // Intent A reappearing after B is a NEW commit (`Ops: 1`), same Git-Source.
    assert!(cs[2].body.contains("\nOps: 1\n"), "{}", cs[2].body);
    assert!(cs[2].body.contains(&format!("\nGit-Source: {SRC_A}\n")));
    assert!(cs[2].body.contains(&format!("\nOp: {}\n", ops[8].op_id)));
    assert!(!cs[2].body.contains("Files:"), "the SetFiles belongs to the first run only");

    // A SetFiles op folded into the group: the FIRST commit's tree already
    // holds every function of the run and the files, exec bit included.
    let ls = git(out, &["ls-tree", "-r", &cs[0].sha]);
    assert!(ls.contains("100644 blob") && ls.contains("\tREADME.md"), "{ls}");
    assert!(ls.contains("100755 blob") && ls.contains("\tbin/run.sh"), "{ls}");
    let src = git(out, &["show", &format!("{}:src.lex", cs[0].sha)]);
    for n in 1..=5 {
        assert!(src.contains(&format!("fn f{n}(")), "f{n} in the first commit: {src}");
    }
    assert!(!src.contains("fn f6("), "B's functions come in the next commit");
    let src_b = git(out, &["show", &format!("{}:src.lex", cs[1].sha)]);
    assert!(src_b.contains("fn f6(") && src_b.contains("fn f7("));
    // The files are still there after later commits (the manifest carries forward).
    assert_eq!(std::fs::read_to_string(out.join("README.md")).unwrap(), "hello\n");

    // Sanity: the log has exactly the 5 commits the assertions above index.
    assert_eq!(git(out, &["rev-list", "--count", "HEAD"]).trim(), "5");
    let _ = &s.ib;
}

#[test]
fn a_native_intent_shared_by_many_ops_keeps_one_commit_per_op() {
    // Even though both native ops share an intent, they must NOT group.
    let s = scenario();
    let cs = commits(s.out.path());
    assert_ne!(cs[3].sha, cs[4].sha);
    assert_eq!(cs.len(), 5);
}

// ── 2. the verbatim message survives the trailer block ─────────────────────

#[test]
fn the_message_is_verbatim_and_the_trailers_still_parse() {
    let s = scenario();
    let out = s.out.path();
    let cs = commits(out);
    let ops = s.fx.ops();

    // `%B` is the prompt byte for byte (minus its final newline, which the
    // blank line before the trailers replaces) + one blank line + trailers.
    let verbatim = ADVERSARIAL.trim_end_matches('\n');
    let want = format!(
        "{verbatim}\n\nOp: {}\nIntent: {}\nFiles: {}\nGit-Source: {SRC_A}\nOps: 6\n",
        ops[5].op_id,
        s.ia.intent_id,
        cs[0]
            .body
            .lines()
            .find_map(|l| l.strip_prefix("Files: "))
            .expect("Files trailer"),
    );
    assert_eq!(cs[0].body.trim_end_matches('\n'), want.trim_end_matches('\n'));
    // Not normalized: the triple blank line, the indented line and the emoji
    // are exactly as written (git's default cleanup would collapse/strip them).
    assert!(cs[0].body.contains("Segundo párrafo con emoji 🚀.\n  indented line kept\n\n\nTriple blank"));

    // The forged trailers in the message's own last paragraph do not leak into
    // the trailer block: git reads trailers from the LAST paragraph only.
    let trailers = git(out, &["log", "-1", "--format=%(trailers:only,unfold)", &cs[0].sha]);
    let keys: Vec<&str> = trailers
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(keys, ["Op", "Intent", "Files", "Git-Source", "Ops"], "{trailers}");
    let git_source = git(out, &["log", "-1", "--format=%(trailers:key=Git-Source,valueonly)", &cs[0].sha]);
    assert_eq!(git_source.trim(), SRC_A, "not the forged deadbeef one");
    let op = git(out, &["log", "-1", "--format=%(trailers:key=Op,valueonly)", &cs[0].sha]);
    assert_eq!(op.trim(), ops[5].op_id);

    // The same through `git interpret-trailers` (which needs --no-divider,
    // else it stops at the message's own `---` line).
    let msg = git(out, &["log", "-1", "--format=%B", &cs[0].sha]);
    let mut child = Command::new("git")
        .args(["interpret-trailers", "--parse", "--no-divider"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), msg.as_bytes()).unwrap();
    let parsed = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let parsed_keys: Vec<&str> = parsed.lines().map(|l| l.split(':').next().unwrap()).collect();
    assert_eq!(parsed_keys, ["Op", "Intent", "Files", "Git-Source", "Ops"], "{parsed}");
}

#[test]
fn a_blank_prompt_still_yields_a_parseable_commit() {
    let work = tempdir().unwrap();
    let mut fx = Fx::new(work.path());
    let i = intent(
        "\n \n",
        "git-import:root",
        Some(origin(SRC_A, person("E", "e@example.com", 1_700_000_000, "+0000"), None)),
    );
    fx.add_fns(&i, 1);
    let out = tempdir().unwrap();
    export(&fx.root, out.path());
    let cs = commits(out.path());
    assert_eq!(cs.len(), 1);
    assert!(cs[0].body.starts_with("(empty prompt)\n\nOp: "), "{}", cs[0].body);
    let src = git(out.path(), &["log", "-1", "--format=%(trailers:key=Git-Source,valueonly)"]);
    assert_eq!(src.trim(), SRC_A);
}

// ── 3. the user's git config cannot leak in or break the export ────────────

#[test]
fn local_git_config_cannot_leak_into_or_break_origin_commits() {
    let work = tempdir().unwrap();
    let mut fx = Fx::new(work.path());
    let i = intent(
        "a message\n",
        "git-import:root",
        Some(origin(
            SRC_A,
            person("Real Author", "real@example.com", 1_700_000_000, "+0200"),
            None,
        )),
    );
    fx.add_fns(&i, 2);

    // A hostile global config: its own identity, signing turned on with a
    // signer that always fails, and a commit-msg hook that rejects everything.
    let cfg_dir = tempdir().unwrap();
    let hooks = cfg_dir.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("commit-msg");
    std::fs::write(&hook, "#!/bin/sh\necho hook ran >&2\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = cfg_dir.path().join("gitconfig");
    std::fs::write(
        &cfg,
        format!(
            "[user]\n\tname = Leaky Local\n\temail = leak@local\n\tsigningkey = ABCDEF\n\
             [commit]\n\tgpgsign = true\n[gpg]\n\tprogram = /bin/false\n\
             [core]\n\thooksPath = {}\n",
            hooks.display()
        ),
    )
    .unwrap();

    let out = tempdir().unwrap();
    let res = export_with(&fx.root, out.path(), &cfg);
    assert!(res.status.success(), "export must not fail on the user's config: {}",
        String::from_utf8_lossy(&res.stderr));
    let cs = commits(out.path());
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].author, "Real Author|real@example.com|2023-11-15T00:13:20+02:00");
    assert_eq!(cs[0].committer, cs[0].author);
    assert!(!git(out.path(), &["log", "-1", "--format=%GG"]).contains("gpg"), "unsigned");
}

// ── 4. edge inputs the source VCS can legally produce ─────────────────────

#[test]
fn awkward_identities_and_dates_still_export() {
    let work = tempdir().unwrap();
    let mut fx = Fx::new(work.path());
    // Empty name (git refuses it), a small timestamp, a malformed zone that
    // git would silently replace with the LOCAL zone, and a pre-epoch moment.
    let a = intent(
        "empty name\n",
        "s",
        Some(origin("a".repeat(40).as_str(), person("", "noname@example.com", 5, "+0000"), None)),
    );
    let b = intent(
        "bad zone\n",
        "s",
        Some(origin("b".repeat(40).as_str(), person("Z", "z@example.com", 1_700_000_000, "CEST"), None)),
    );
    let c = intent(
        "before the epoch\n",
        "s",
        Some(origin("c".repeat(40).as_str(), person("P", "p@example.com", 0, "-0100"), None)),
    );
    fx.add_fns(&a, 1);
    fx.add_fns(&b, 1);
    fx.add_fns(&c, 1);
    let out = tempdir().unwrap();
    let res = export_with(&fx.root, out.path(), Path::new("/dev/null"));
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));
    let cs = commits(out.path());
    assert_eq!(cs.len(), 3);
    assert_eq!(cs[0].author, "noname@example.com|noname@example.com|1970-01-01T00:00:05Z");
    assert_eq!(cs[1].author, "Z|z@example.com|2023-11-14T22:13:20Z", "malformed zone => +0000, never local");
    assert_eq!(cs[2].author, "P|p@example.com|1970-01-01T00:00:00Z", "clamped to the epoch");
}

// ── 5. backward compatibility: no origin => byte-identical to before ──────

/// Full `git log` of the fixture below, captured by running the UNMODIFIED
/// `main` binary (v0.11.72 + #909, before this change) with the dates pinned.
/// Commit SHAs cover tree + message + identity + date + parent, so equality
/// here means the export is byte-identical, not merely similar. If a change to
/// canonicalization / op hashing legitimately moves the op/intent/manifest ids
/// in it, regenerate with `main` and note why in the PR.
const BASELINE_LOG: &str = include_str!("export_git_origin_892_baseline.txt");

fn lex_in(dir: &Path, home: &Path, args: &[&str]) {
    let out = Command::new(lex_bin())
        .current_dir(dir)
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "lex {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn a_store_without_origins_exports_byte_identically_to_main() {
    let work = tempdir().unwrap();
    let home = tempdir().unwrap();
    let pkg = work.path().join("pkg");
    let store = work.path().join("store");
    let (pkg_s, store_s) = (pkg.to_str().unwrap(), store.to_str().unwrap());

    // A multi-file package with a local aliased import, plus non-.lex files
    // (=> SetFiles ops), published three times with different intents. The
    // session is pinned: the default (`cli-<pid>-<epoch>`) would make the op
    // ids differ run to run.
    write(&pkg, "lex.toml", "[package]\nname = \"fxpkg\"\nversion = \"0.1.0\"\n");
    write(&pkg, "src/error.lex", "type Err = { code :: Int, msg :: Str }\n\nfn format(e :: Err) -> Str {\n  e.msg\n}\n");
    write(&pkg, "src/main.lex", "import \"./error\" as e\n\nfn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n");
    write(&pkg, "README.md", "# fxpkg\n");
    write(&pkg, "docs/data.csv", "a,b\n1,2\n");
    lex_in(work.path(), home.path(), &["publish", pkg_s, "--store", store_s, "--activate",
        "--intent-prompt", "initial import of fxpkg", "--intent-session", "fx-1", "--intent-model", "a/b"]);

    let main = std::fs::read_to_string(pkg.join("src/main.lex")).unwrap();
    write(&pkg, "src/main.lex", &format!("{main}\nfn twice(x :: e.Err) -> Str {{\n  e.format(x)\n}}\n"));
    write(&pkg, "README.md", "# fxpkg v2\n\nmore\n");
    lex_in(work.path(), home.path(), &["publish", pkg_s, "--store", store_s, "--activate",
        "--intent-prompt", "add twice()\n\nwith a second paragraph", "--intent-session", "fx-2", "--intent-model", "a/b"]);

    write(&pkg, "docs/data.csv", "a,b\n1,2\nx,y\n");
    lex_in(work.path(), home.path(), &["publish", pkg_s, "--store", store_s, "--activate", "--intent-session", "fx-3"]);

    // Pin both dates: the pre-#892 exporter stamps the CURRENT time.
    let out = work.path().join("out");
    let res = Command::new(lex_bin())
        .env("HOME", home.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .args(["export-git", out.to_str().unwrap(), "--store", store_s])
        .output()
        .unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    let log = git(&out, &["log", "--reverse", "--format=%H%n%an|%ae|%aI|%cn|%ce|%cI%n%B%n--END--"]);
    assert_eq!(log, BASELINE_LOG, "export of an origin-free store drifted from main");
    assert_eq!(git(&out, &["rev-parse", "HEAD^{tree}"]).trim(), "dcb9cd9b18737e7b0087e3ec12b47ca93bac455f");
    assert_eq!(git(&out, &["rev-list", "--count", "HEAD"]).trim(), "8");
    assert_eq!(
        git(&out, &["ls-files"]),
        "README.md\ndocs/data.csv\nlex.toml\nsrc/error.lex\nsrc/main.lex\n"
    );
}
