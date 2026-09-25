//! #892 PR 1: the shared publish core (`publish_dir`) is a pure refactor of
//! `cmd_publish` — `lex publish <dir>` must not change by a single byte.
//!
//! Every `GOLDEN_*` literal below was captured by running THIS test's scenarios
//! against the ORIGINAL (pre-refactor) `lex` binary built from `main` at
//! 053bf79, with `LEX_GOLDEN_PRINT=1` (which prints each transcript instead of
//! asserting). After the refactor the same scenarios must reproduce them
//! exactly: stdout of `lex publish --output json`, the op log, the files
//! manifest listing, the committed lock, the examples attestations, the exit
//! codes and the stderr text of every failure mode.
//!
//! Determinism: op ids hash `(kind, parents, intent_id)`, and the intent id
//! hashes `(prompt, session, model, ...)` but NOT `created_at`, so every
//! scenario pins `--intent-prompt/--intent-session/--intent-model`.
//!
//! To re-capture (only ever against a binary whose behaviour you trust):
//!   LEX_GOLDEN_PRINT=1 cargo test -p lex-cli --test publish_core_892 -- --nocapture --test-threads=1

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn write(dir: &Path, name: &str, contents: &[u8]) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, contents).unwrap();
}

const ERROR_LEX: &str = "type Err = { code :: Int, msg :: Str }\n\n\
    fn format(e :: Err) -> Str {\n  e.msg\n}\n";
const LIB_LEX: &str = "import \"./error\" as e\n\n\
    fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n\n\
    fn double(n :: Int) -> Int\n  examples { double(2) => 4 }\n{\n  n * 2\n}\n";
const LOCK: &str = "# lex.lock\n[[package]]\nname = \"op_v1\"\n";

/// The synthetic multi-file package the whole suite runs on:
/// `src/lib.lex` + `src/error.lex` (local alias import), `lex.toml`,
/// `lex.lock`, README, `tests/`, an exec-bit file and a binary file.
fn fixture(pkg: &Path) {
    write(pkg, "lex.toml", b"[package]\nname = \"goldpkg\"\nversion = \"0.1.0\"\n");
    write(pkg, "lex.lock", LOCK.as_bytes());
    write(pkg, "src/error.lex", ERROR_LEX.as_bytes());
    write(pkg, "src/lib.lex", LIB_LEX.as_bytes());
    write(pkg, "README.md", b"# goldpkg\n");
    write(pkg, "tests/t.txt", b"t\n");
    write(pkg, "bin/run.sh", b"#!/bin/sh\necho hi\n");
    write(pkg, "logo.bin", &[0x00, 0x01, 0x02, 0xff, 0xfe]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = pkg.join("bin/run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

struct Ctx {
    tmp: tempfile::TempDir,
}

impl Ctx {
    fn new() -> Self {
        Ctx { tmp: tempfile::tempdir().unwrap() }
    }
    fn pkg(&self) -> PathBuf {
        self.tmp.path().join("pkg")
    }
    fn store(&self) -> PathBuf {
        self.tmp.path().join("store")
    }
    fn norm(&self, s: &str) -> String {
        let root = self.tmp.path().to_string_lossy().into_owned();
        // macOS: /var/... is reached via /private/var/... in some messages.
        s.replace(&format!("/private{root}"), "<TMP>").replace(&root, "<TMP>")
    }
    /// `lex <args>` run from the tmp root (so relative paths in messages are stable).
    fn run(&self, args: &[&str]) -> Output {
        lex()
            .current_dir(self.tmp.path())
            .env_remove("LEX_INTENT_SESSION")
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
    }
    fn publish(&self, dir: &str, extra: &[&str]) -> Output {
        let store = self.store();
        let mut args: Vec<&str> = vec!["--output", "json", "publish", "--store", store.to_str().unwrap()];
        args.extend_from_slice(INTENT);
        args.extend_from_slice(extra);
        args.push(dir);
        self.run(&args)
    }
    /// The normalized `data` object of a JSON envelope on stdout.
    fn data_of(&self, out: &Output) -> String {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut v: serde_json::Value = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("non-JSON stdout: {e}\n{text}\nstderr: {}", String::from_utf8_lossy(&out.stderr)));
        if let Some(o) = v.as_object_mut() {
            o.remove("meta"); // duration_ms is wall-clock
        }
        self.norm(&serde_json::to_string(&v).unwrap())
    }
    fn op_log(&self) -> String {
        let store = self.store();
        let out = self.run(&["--output", "json", "op", "log", "--store", store.to_str().unwrap()]);
        assert!(out.status.success(), "op log: {}", String::from_utf8_lossy(&out.stderr));
        self.data_of(&out)
    }
    fn files_ls(&self) -> String {
        let store = self.store();
        let out = self.run(&["files", "ls", "--store", store.to_str().unwrap()]);
        assert!(out.status.success(), "files ls: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
    fn committed_lock(&self, head: &str) -> String {
        format!("{:?}", lex_store::Store::open(self.store()).unwrap().committed_lock(head).unwrap())
    }
    /// The `Examples` attestations, as sorted `(stage, op, count, result)` lines.
    fn examples_attestations(&self) -> String {
        let store = lex_store::Store::open(self.store()).unwrap();
        let mut rows: Vec<String> = store
            .attestation_log()
            .unwrap()
            .list_all()
            .unwrap()
            .iter()
            .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::Examples { .. }))
            .map(|a| {
                format!(
                    "{} {:?} {} {}",
                    a.stage_id,
                    a.op_id,
                    serde_json::to_string(&a.kind).unwrap(),
                    serde_json::to_string(&a.result).unwrap()
                )
            })
            .collect();
        rows.sort();
        rows.join("\n")
    }
}

const INTENT: &[&str] = &[
    "--intent-prompt", "golden publish", "--intent-session", "golden-session", "--intent-model", "test/model",
];

/// Print (capture mode) or assert (default) one transcript.
fn golden(name: &str, actual: &str, expected: &str) {
    if std::env::var_os("LEX_GOLDEN_PRINT").is_some() {
        eprintln!("=====GOLDEN {name}=====\n{actual}\n=====END {name}=====");
        return;
    }
    assert_eq!(actual, expected, "golden `{name}` drifted: the publish path changed behaviour");
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn head_of(data: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(data).unwrap();
    v["data"]["head_op"].as_str().unwrap().to_string()
}

// ── the multi-file package: first publish, republish, edits ─────────────────

#[test]
fn golden_multifile_publish_and_republish() {
    let c = Ctx::new();
    fixture(&c.pkg());

    let first = c.publish("pkg", &[]);
    ok(&first);
    let d1 = c.data_of(&first);
    golden("publish.first", &d1, GOLDEN_PUBLISH_FIRST);
    let head1 = head_of(&d1);
    golden("publish.first.head", &head1, GOLDEN_HEAD_FIRST);
    golden("oplog.first", &c.op_log(), GOLDEN_OPLOG_FIRST);
    golden("files.ls", &c.files_ls(), GOLDEN_FILES_LS);
    golden("lock.first", &c.committed_lock(&head1), GOLDEN_LOCK_FIRST);
    golden("examples.first", &c.examples_attestations(), GOLDEN_EXAMPLES_FIRST);

    // Unchanged republish: zero ops, same head, and the head's committed lock
    // is left alone (#1036 / #1007 §0).
    let again = c.publish("pkg", &[]);
    ok(&again);
    let d2 = c.data_of(&again);
    golden("publish.unchanged", &d2, GOLDEN_PUBLISH_UNCHANGED);
    assert_eq!(head_of(&d2), head1, "an unchanged republish must not move the head");
    golden("lock.after_noop", &c.committed_lock(&head1), GOLDEN_LOCK_AFTER_NOOP);
    assert!(c.committed_lock(&head1).contains("op_v1"));

    // A republish whose only change is the lockfile is NOT a no-op (the lock
    // is part of the files manifest): it produces a SetFiles op, and only THAT
    // new head gets the new lock; the old head keeps the one it was built with.
    write(&c.pkg(), "lex.lock", b"# drifted\n[[package]]\nname = \"other\"\n");
    let lock_edit = c.publish("pkg", &[]);
    ok(&lock_edit);
    let dl = c.data_of(&lock_edit);
    golden("publish.lock_edit", &dl, GOLDEN_PUBLISH_LOCK_EDIT);
    golden("lock.old_head_after_lock_edit", &c.committed_lock(&head1), GOLDEN_LOCK_OLD_HEAD);
    golden("lock.new_head", &c.committed_lock(&head_of(&dl)), GOLDEN_LOCK_NEW_HEAD);

    // Files-only edit: exactly one SetFiles op, zero semantic ops.
    write(&c.pkg(), "README.md", b"# goldpkg v2\n");
    let files_only = c.publish("pkg", &[]);
    ok(&files_only);
    let d3 = c.data_of(&files_only);
    golden("publish.files_only", &d3, GOLDEN_PUBLISH_FILES_ONLY);
    // The produced head carries the lock as it is on disk now.
    golden("lock.files_only", &c.committed_lock(&head_of(&d3)), GOLDEN_LOCK_FILES_ONLY);

    // Semantic edit (+ files unchanged since): a ModifyBody on `double`.
    write(
        &c.pkg(),
        "src/lib.lex",
        LIB_LEX.replace("n * 2", "n + n").as_bytes(),
    );
    let sem = c.publish("pkg", &[]);
    ok(&sem);
    let d4 = c.data_of(&sem);
    golden("publish.semantic", &d4, GOLDEN_PUBLISH_SEMANTIC);
    golden("oplog.final", &c.op_log(), GOLDEN_OPLOG_FINAL);
    golden("examples.final", &c.examples_attestations(), GOLDEN_EXAMPLES_FINAL);
}

// ── flags ───────────────────────────────────────────────────────────────────

#[test]
fn golden_no_files() {
    let c = Ctx::new();
    fixture(&c.pkg());
    let out = c.publish("pkg", &["--no-files"]);
    ok(&out);
    golden("publish.no_files", &c.data_of(&out), GOLDEN_PUBLISH_NO_FILES);
    golden("files.ls.no_files", &c.files_ls(), GOLDEN_FILES_LS_EMPTY);
}

#[test]
fn golden_dry_run() {
    let c = Ctx::new();
    fixture(&c.pkg());
    // `--dry-run` reports the plan and exits 9 (the ACLI dry-run code).
    let out = c.publish("pkg", &["--dry-run"]);
    assert_eq!(out.status.code(), Some(9), "dry-run exit code");
    golden("publish.dry_run", &c.data_of(&out), GOLDEN_PUBLISH_DRY_RUN);
    // A dry run opens (and so creates) the store but records no op.
    golden(
        "dry_run.effects",
        &format!("store_exists={} oplog={}", c.store().exists(), c.op_log()),
        GOLDEN_DRY_RUN_EFFECTS,
    );
    // Text mode: summary + one line per planned action on stderr.
    let store = c.store();
    let out = c.run(&["publish", "--dry-run", "--store", store.to_str().unwrap(), "pkg"]);
    assert_eq!(out.status.code(), Some(9));
    golden("dry_run.text.stdout", &c.norm(&String::from_utf8_lossy(&out.stdout)), GOLDEN_EMPTY);
    golden("dry_run.text.stderr", &c.norm(&String::from_utf8_lossy(&out.stderr)), GOLDEN_DRY_RUN_TEXT);
    // Dry-running an already-published package plans zero ops.
    ok(&c.publish("pkg", &[]));
    let out = c.publish("pkg", &["--dry-run"]);
    assert_eq!(out.status.code(), Some(9));
    golden("publish.dry_run.unchanged", &c.data_of(&out), GOLDEN_PUBLISH_DRY_RUN_UNCHANGED);
}

#[test]
fn golden_signed_publish() {
    let c = Ctx::new();
    fixture(&c.pkg());
    let out = c.publish(
        "pkg",
        &["--signing-key", "0101010101010101010101010101010101010101010101010101010101010101"],
    );
    ok(&out);
    golden("publish.signed", &c.data_of(&out), GOLDEN_PUBLISH_SIGNED);
}

#[test]
fn golden_branch_flag() {
    let c = Ctx::new();
    fixture(&c.pkg());
    // A branch must exist before it can be published to.
    let out = c.publish("pkg", &["--branch", "feat"]);
    assert!(!out.status.success());
    golden("publish.branch.unknown", &c.data_of(&out), GOLDEN_PUBLISH_BRANCH_UNKNOWN);
    let store = c.store();
    ok(&c.run(&["--output", "json", "branch", "create", "feat", "--store", store.to_str().unwrap()]));
    let out = c.publish("pkg", &["--branch", "feat"]);
    ok(&out);
    golden("publish.branch", &c.data_of(&out), GOLDEN_PUBLISH_BRANCH);
}

#[test]
fn golden_single_file_publish() {
    let c = Ctx::new();
    write(c.tmp.path(), "one.lex", b"fn one(x :: Int) -> Int {\n  x + 1\n}\n");
    let out = c.publish("one.lex", &[]);
    ok(&out);
    golden("publish.single_file", &c.data_of(&out), GOLDEN_PUBLISH_SINGLE_FILE);
    // A single-file publish never captures files.
    golden("files.ls.single_file", &c.files_ls(), GOLDEN_FILES_LS_EMPTY);
}

// ── failure modes: exit code + stderr, byte for byte ────────────────────────

#[test]
fn golden_type_error_exit_2() {
    let c = Ctx::new();
    fixture(&c.pkg());
    write(&c.pkg(), "src/bad.lex", b"fn bad(n :: Int) -> Str {\n  n\n}\n");

    let out = c.publish("pkg", &[]);
    assert_eq!(out.status.code(), Some(2), "a type error is exit 2");
    golden("type_error.json", &c.data_of(&out), GOLDEN_TYPE_ERROR_JSON);
    golden("type_error.json.stderr", &c.norm(&String::from_utf8_lossy(&out.stderr)), GOLDEN_EMPTY);

    // Text mode: the diagnostics go to stderr, one JSON object per line.
    let store = c.store();
    let out = c.run(&["publish", "--store", store.to_str().unwrap(), "pkg"]);
    assert_eq!(out.status.code(), Some(2));
    golden("type_error.text.stdout", &c.norm(&String::from_utf8_lossy(&out.stdout)), GOLDEN_EMPTY);
    golden("type_error.text.stderr", &c.norm(&String::from_utf8_lossy(&out.stderr)), GOLDEN_TYPE_ERROR_TEXT);

    // The gate fires BEFORE the store is opened: nothing is created.
    assert!(!store.exists(), "a rejected publish must not create the store");
}

#[test]
fn golden_examples_failure_exit_2() {
    let c = Ctx::new();
    fixture(&c.pkg());
    write(
        &c.pkg(),
        "src/lib.lex",
        LIB_LEX.replace("double(2) => 4", "double(2) => 5").as_bytes(),
    );
    let out = c.publish("pkg", &[]);
    assert_eq!(out.status.code(), Some(2));
    golden("examples_error.json", &c.data_of(&out), GOLDEN_EXAMPLES_ERROR_JSON);
    assert!(!c.store().exists(), "a rejected publish must not create the store");
}

#[test]
fn golden_empty_or_missing_package_errors_unchanged() {
    let c = Ctx::new();
    let store = c.store();
    let store = store.to_str().unwrap();
    let mut lines = Vec::new();
    let mut case = |label: &str, out: Output| {
        lines.push(format!(
            "{label}: exit={:?} stdout={:?} stderr={:?}",
            out.status.code(),
            c.norm(&String::from_utf8_lossy(&out.stdout)),
            c.norm(&String::from_utf8_lossy(&out.stderr)),
        ));
    };
    // No such path.
    case("missing", c.run(&["publish", "--store", store, "pkg_none"]));
    // A directory with no lex.toml.
    std::fs::create_dir_all(c.tmp.path().join("nomanifest")).unwrap();
    case("no_manifest", c.run(&["publish", "--store", store, "nomanifest"]));
    // A manifest but no src/.
    write(c.tmp.path(), "nosrc/lex.toml", b"[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
    case("no_src", c.run(&["publish", "--store", store, "nosrc"]));
    // A manifest and src/ but no .lex files: the user ran it in an empty package by mistake.
    write(c.tmp.path(), "nolex/lex.toml", b"[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
    std::fs::create_dir_all(c.tmp.path().join("nolex/src")).unwrap();
    case("no_lex_files", c.run(&["publish", "--store", store, "nolex"]));
    // A manifest without a [package] name.
    write(c.tmp.path(), "noname/lex.toml", b"[dependencies]\n");
    std::fs::create_dir_all(c.tmp.path().join("noname/src")).unwrap();
    case("no_name", c.run(&["publish", "--store", store, "noname"]));
    // No path argument at all.
    case("usage", c.run(&["publish", "--store", store]));
    // A flag missing its value.
    case("flag_no_value", c.run(&["publish", "--store", store, "pkg", "--branch"]));
    golden("empty_or_missing", &lines.join("\n"), GOLDEN_EMPTY_OR_MISSING);
}

// ── literals captured from the ORIGINAL binary ──────────────────────────────

const GOLDEN_EMPTY: &str = "";

const GOLDEN_DRY_RUN_EFFECTS: &str = r#"store_exists=true oplog={"command":"op","data":{"branch":"main","budget_drift_threshold_pct":null,"log":[]},"ok":true}"#;

const GOLDEN_DRY_RUN_TEXT: &str = r#"dry-run: would apply 5 op(s) to branch main
  • {"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"}
  • {"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"}
  • {"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"}
  • {"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"}
  • {"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"}
"#;

const GOLDEN_EMPTY_OR_MISSING: &str = r#"missing: exit=Some(1) stdout="" stderr="error: read pkg_none: No such file or directory (os error 2): No such file or directory (os error 2)\n"
no_manifest: exit=Some(1) stdout="" stderr="error: reading nomanifest/lex.toml (a package publish needs it): reading nomanifest/lex.toml: No such file or directory (os error 2)\n"
no_src: exit=Some(1) stdout="" stderr="error: package nosrc has no src/ directory to publish\n"
no_lex_files: exit=Some(1) stdout="" stderr="error: no .lex files under nolex/src\n"
no_name: exit=Some(1) stdout="" stderr="error: noname/lex.toml needs a [package] name to publish a package\n"
usage: exit=Some(1) stdout="" stderr="error: usage: lex publish [--store DIR] [--branch NAME] [--activate] [--signing-key HEX] [--intent-prompt TEXT] [--intent-model PROVIDER/NAME] [--intent-session ID] [--intent-issue ISSUE_ID] [--no-files] <file|dir>\n\nEvery publish records an Intent. Without --intent-prompt it is recorded as explicitly unattributed (#970) — pass --intent-prompt to say why the change was made, which is what makes `lex recall` and `lex op replay` useful.\n\nA directory publish also captures the working copy's non-op-log files (README, lex.toml, lex.lock, tests/, ...) as one SetFiles op, last, under the same intent (#1007) — pass --no-files to opt a single publish out.\n"
flag_no_value: exit=Some(1) stdout="" stderr="error: --branch needs a value\n""#;

const GOLDEN_EXAMPLES_ERROR_JSON: &str = r#"{"command":"publish","data":{"errors":[{"at_node":"n_0","case_index":0,"expected":"5","fn_name":"lib_4692594e.double","got":"4","kind":"example_mismatch"}],"phase":"examples"},"ok":true}"#;

const GOLDEN_EXAMPLES_FINAL: &str = r#"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe Some("41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5") {"kind":"examples","file_hash":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","count":1} {"result":"passed"}
3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe Some("d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6") {"kind":"examples","file_hash":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","count":1} {"result":"passed"}
3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe Some("f9250d91caebb6424b6d4be1952aa0c4a7aca4e16026d692fd02766499f84af4") {"kind":"examples","file_hash":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","count":1} {"result":"passed"}
87fb65ab1d1d6fd59c8c0023efafb7e63dda1261f45c28631f901e861c07c7ab Some("25badb984101f7ed17a77186ed1a51e1ab0c839fa42d5f54012372a011537e96") {"kind":"examples","file_hash":"87fb65ab1d1d6fd59c8c0023efafb7e63dda1261f45c28631f901e861c07c7ab","count":1} {"result":"passed"}"#;

const GOLDEN_EXAMPLES_FIRST: &str = r#"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe Some("d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6") {"kind":"examples","file_hash":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","count":1} {"result":"passed"}"#;

const GOLDEN_FILES_LS: &str = r#"100644	10	612fed2d4890991dd0cdf66906234ef729ad70b7d8e5b08eb589aa1882adbed6	README.md
100755	18	299001868fb8c02fd431c336c6d058f5558c5dff5b5af5e6fe04b870a6a9cbba	bin/run.sh
100644	38	62baed86bb1365a84d43c32c0df72adfd5e0cdcd1bf3a8d5b294f902cc0cc40d	lex.lock
100644	45	8ed7a615606267ef958dc3c207256274af0efda14235bbf77247444488a24803	lex.toml
100644	5	aa5cd9acfab25f643fb1cedb67f8770417ac9ce0b02cfe72a62fa1ec20e9f60a	logo.bin
100644	2	fe8edeeb98cc6d3b93cf2d57000254b84bd9eba34b4df7ce4b87db8b937b7703	tests/t.txt
"#;

const GOLDEN_FILES_LS_EMPTY: &str = r#""#;

const GOLDEN_HEAD_FIRST: &str = r#"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5"#;

const GOLDEN_LOCK_AFTER_NOOP: &str = r##"Some("# lex.lock\n[[package]]\nname = \"op_v1\"\n")"##;

const GOLDEN_LOCK_FILES_ONLY: &str = r##"Some("# drifted\n[[package]]\nname = \"other\"\n")"##;

const GOLDEN_LOCK_FIRST: &str = r##"Some("# lex.lock\n[[package]]\nname = \"op_v1\"\n")"##;

const GOLDEN_LOCK_NEW_HEAD: &str = r##"Some("# drifted\n[[package]]\nname = \"other\"\n")"##;

const GOLDEN_LOCK_OLD_HEAD: &str = r##"Some("# lex.lock\n[[package]]\nname = \"op_v1\"\n")"##;

const GOLDEN_OPLOG_FINAL: &str = r#"{"command":"op","data":{"branch":"main","budget_drift_threshold_pct":null,"log":[{"from_stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"modify_body","op_id":"25badb984101f7ed17a77186ed1a51e1ab0c839fa42d5f54012372a011537e96","parents":["2fcc714f2e67b70f436cdfd3a983edce76c18e50dd6b266658272d1949566f3a"],"produces":{"from":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","kind":"replace","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","to":"87fb65ab1d1d6fd59c8c0023efafb7e63dda1261f45c28631f901e861c07c7ab"},"sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","to_stage_id":"87fb65ab1d1d6fd59c8c0023efafb7e63dda1261f45c28631f901e861c07c7ab"},{"intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","manifest":"5ba036c7ad9190c1d74133efea3f1942820734c21ee2431a0dda8d89625df28e","op":"set_files","op_id":"2fcc714f2e67b70f436cdfd3a983edce76c18e50dd6b266658272d1949566f3a","parents":["f9250d91caebb6424b6d4be1952aa0c4a7aca4e16026d692fd02766499f84af4"],"produces":{"kind":"files_only"}},{"intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","manifest":"463eec38418abd2f39054e9db8630270f4c586b6dcb5fcfc0e6983129de59f82","op":"set_files","op_id":"f9250d91caebb6424b6d4be1952aa0c4a7aca4e16026d692fd02766499f84af4","parents":["41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5"],"produces":{"kind":"files_only"}},{"intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","op":"set_files","op_id":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","parents":["5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"],"produces":{"kind":"files_only"}},{"in_file":"src/error.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_type","op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981","parents":["84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"],"produces":{"kind":"create","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},{"effects":[],"in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12","parents":["d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"],"produces":{"kind":"create","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},{"effects":[],"in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6","parents":["e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"],"produces":{"kind":"create","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},{"effects":[],"in_file":"src/error.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70","parents":["9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"],"produces":{"kind":"create","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},{"alias":"e","in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","module":"./error","op":"add_import","op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512","produces":{"kind":"import_only"}}]},"ok":true}"#;

const GOLDEN_OPLOG_FIRST: &str = r#"{"command":"op","data":{"branch":"main","budget_drift_threshold_pct":null,"log":[{"intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","op":"set_files","op_id":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","parents":["5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"],"produces":{"kind":"files_only"}},{"in_file":"src/error.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_type","op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981","parents":["84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"],"produces":{"kind":"create","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},{"effects":[],"in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12","parents":["d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"],"produces":{"kind":"create","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},{"effects":[],"in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6","parents":["e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"],"produces":{"kind":"create","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},{"effects":[],"in_file":"src/error.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","op":"add_function","op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70","parents":["9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"],"produces":{"kind":"create","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},{"alias":"e","in_file":"src/lib.lex","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","module":"./error","op":"add_import","op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512","produces":{"kind":"import_only"}}]},"ok":true}"#;

const GOLDEN_PUBLISH_BRANCH: &str = r#"{"command":"publish","data":{"files_manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","head_op":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"},"op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"},{"kind":{"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"},{"kind":{"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"},{"kind":{"manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","op":"set_files"},"op_id":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_BRANCH_UNKNOWN: &str = r#"{"command":"publish","error":{"code":"GENERAL_ERROR","message":"unknown branch `feat`"},"ok":false}"#;

const GOLDEN_PUBLISH_DRY_RUN: &str = r#"{"command":"publish","data":null,"dry_run":true,"ok":true,"planned_actions":[{"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"},{"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},{"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"}]}"#;

const GOLDEN_PUBLISH_DRY_RUN_UNCHANGED: &str = r#"{"command":"publish","data":null,"dry_run":true,"ok":true,"planned_actions":[]}"#;

const GOLDEN_PUBLISH_FILES_ONLY: &str = r#"{"command":"publish","data":{"files_manifest":"5ba036c7ad9190c1d74133efea3f1942820734c21ee2431a0dda8d89625df28e","head_op":"2fcc714f2e67b70f436cdfd3a983edce76c18e50dd6b266658272d1949566f3a","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"manifest":"5ba036c7ad9190c1d74133efea3f1942820734c21ee2431a0dda8d89625df28e","op":"set_files"},"op_id":"2fcc714f2e67b70f436cdfd3a983edce76c18e50dd6b266658272d1949566f3a"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_FIRST: &str = r#"{"command":"publish","data":{"files_manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","head_op":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"},"op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"},{"kind":{"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"},{"kind":{"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"},{"kind":{"manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","op":"set_files"},"op_id":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_LOCK_EDIT: &str = r#"{"command":"publish","data":{"files_manifest":"463eec38418abd2f39054e9db8630270f4c586b6dcb5fcfc0e6983129de59f82","head_op":"f9250d91caebb6424b6d4be1952aa0c4a7aca4e16026d692fd02766499f84af4","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"manifest":"463eec38418abd2f39054e9db8630270f4c586b6dcb5fcfc0e6983129de59f82","op":"set_files"},"op_id":"f9250d91caebb6424b6d4be1952aa0c4a7aca4e16026d692fd02766499f84af4"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_NO_FILES: &str = r#"{"command":"publish","data":{"files_manifest":null,"head_op":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"},"op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"},{"kind":{"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"},{"kind":{"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_SEMANTIC: &str = r#"{"command":"publish","data":{"files_manifest":null,"head_op":"25badb984101f7ed17a77186ed1a51e1ab0c839fa42d5f54012372a011537e96","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"from_stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe","op":"modify_body","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","to_stage_id":"87fb65ab1d1d6fd59c8c0023efafb7e63dda1261f45c28631f901e861c07c7ab"},"op_id":"25badb984101f7ed17a77186ed1a51e1ab0c839fa42d5f54012372a011537e96"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_SIGNED: &str = r#"{"command":"publish","data":{"files_manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","head_op":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"alias":"e","in_file":"src/lib.lex","module":"./error","op":"add_import"},"op_id":"9276736eff9d4adf87326ab146f9ce95dc7535ff0a441b043c727483c6c33512"},{"kind":{"effects":[],"in_file":"src/error.lex","op":"add_function","sig_id":"4467a76a6f547138aa2434ec63f71aa4dfb9134325ebb3b21151a44911befdd8","stage_id":"57fc47f9b273c69aa17a6ab9ee35f1be3a4512630b4f585d71ef90a7b7ad9900"},"op_id":"e1e51f27ed3e44455520ca60275cbc530257bd5f687141da22ea92c1c0a0dc70"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"e21b07b97e20c3c60cc7b500620ca68fc6d8d34d18d5b21733bb917536241e18","stage_id":"3317415522642ccfb02cf29f70bd61fa8279f8ad31b15d753bdb1e9840bb55fe"},"op_id":"d89a636183db19c22ccc94fecd117c7f656eacf8e09bf3e356b1097fa4f58ec6"},{"kind":{"effects":[],"in_file":"src/lib.lex","op":"add_function","sig_id":"3aa606390436ee5d6cff704019d4cb2b2b24f096deb42573671eff9f64ea6e81","stage_id":"241ca093784c273d179abfda037bb64aa403815467510119d18fc49eced29430"},"op_id":"84eae8fba3a2310c203f1e1e95739e0f23a3c54c15b3604b79a778302a984d12"},{"kind":{"in_file":"src/error.lex","op":"add_type","sig_id":"779da4bc9c8a22935ab42260d1447b4c956827e3e8e0026eb0a639e51d122c88","stage_id":"4f304d4300d8097f9cd62111675909a0df983a054968cb4169e39a6784da41bc"},"op_id":"5ed60190d9fce29c51fe3f86a8907c645e36b853245bed0deea0fdb27e40a981"},{"kind":{"manifest":"e59d63b27468e292e0dd18f6c7b2d63ea5498912048e1e7fa532bba6314160e8","op":"set_files"},"op_id":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5"}],"signed_by":"8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"},"ok":true}"#;

const GOLDEN_PUBLISH_SINGLE_FILE: &str = r#"{"command":"publish","data":{"files_manifest":null,"head_op":"403bcc00448255b7333116a4dc68b81404331e8e6cd267d261a0f3e543d5d379","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[{"kind":{"effects":[],"op":"add_function","sig_id":"bedae6c2eab69df80dcbe2427fc0e69f74745210d408e3992cd746899cd298bc","stage_id":"4953078ff4e7a8a8bc597b34042b439af2ec33d313cb5dd0b329a1f41658915b"},"op_id":"403bcc00448255b7333116a4dc68b81404331e8e6cd267d261a0f3e543d5d379"}],"signed_by":null},"ok":true}"#;

const GOLDEN_PUBLISH_UNCHANGED: &str = r#"{"command":"publish","data":{"files_manifest":null,"head_op":"41a563e7815a2c80adb264c6cc7db335991a21ec2e097bc3f7ce43b368a1edb5","intent_id":"8531fbc06cca27fb9b706cdccba77629b23a7930bec65b9f74abedb73dcfb033","ops":[],"signed_by":null},"ok":true}"#;

const GOLDEN_TYPE_ERROR_JSON: &str = r#"{"command":"publish","data":{"errors":[{"at_node":"n_0","context":["in function `bad_a0ca867f.bad`"],"expected":"Str","got":"Int","kind":"type_mismatch"}],"phase":"type-check"},"ok":true}"#;

const GOLDEN_TYPE_ERROR_TEXT: &str = r#"{"kind":"type_mismatch","at_node":"n_0","expected":"Str","got":"Int","context":["in function `bad_a0ca867f.bad`"]}
"#;
