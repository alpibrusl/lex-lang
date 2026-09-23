//! #1007 PR 4: `lex publish <pkgdir>` capturing the working copy into a
//! files manifest, and the `lex files` command group.
//!
//! Covers: README/tests/executable-bit/binary all captured; an unchanged
//! republish is a true no-op (zero ops, no SetFiles); a files-only edit
//! produces exactly one SetFiles op and zero semantic ops; `src/**/*.lex`
//! never appears in the manifest even when git-tracked at that path; a
//! gitignored file is excluded while a force-added one is included; a
//! no-op republish does not rewrite the head's committed lock (§0's fix);
//! and `lex files status|ls|cat|checkout` round-trip against a real
//! published store.

use std::path::Path;
use std::process::{Command, Output};

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn write(dir: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, contents).unwrap();
    p
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    lex()
        .current_dir(cwd)
        .args(["--output", "json"])
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
}

fn ok(cwd: &Path, args: &[&str]) -> Output {
    let out = run(cwd, args);
    assert!(
        out.status.success(),
        "`lex {}` failed:\nstdout: {}\nstderr: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn json(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!("non-JSON output: {e}\nstdout: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    })
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

fn ops_of(v: &serde_json::Value) -> Vec<serde_json::Value> {
    data(v)["ops"].as_array().cloned().unwrap_or_default()
}

fn op_kinds(v: &serde_json::Value) -> Vec<String> {
    ops_of(v)
        .iter()
        .filter_map(|o| o.pointer("/kind/op").and_then(|k| k.as_str()).map(str::to_string))
        .collect()
}

fn scaffold(pkg: &Path, name: &str) {
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    write(pkg, "lex.toml", format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n").as_bytes());
    write(&pkg.join("src"), "a.lex", b"fn f(x :: Int) -> Int { x }\n");
}

fn store_of(pkg: &Path) -> std::path::PathBuf {
    pkg.join(".lex/store")
}

// ── capture: README / tests / exec bit / binary ─────────────────────────────

#[test]
fn directory_publish_captures_readme_tests_exec_bit_and_binary() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "captures");
    write(&pkg, "README.md", b"# captures\n");
    write(&pkg.join("tests"), "smoke.lex", b"fn t() -> Bool { true }\n");
    let bin = write(&pkg, "logo.bin", &[0x00, 0xff, 0x89, 0x50, 0x4e, 0x47, 0x0a, 0xfe]);
    let script = write(&pkg, "run.sh", b"#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let store = store_of(&pkg);
    let pub_out = ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "--activate", "."]);
    let kinds = op_kinds(&json(&pub_out));
    assert!(kinds.contains(&"set_files".to_string()), "expected a set_files op, got {kinds:?}");

    let ls = json(&ok(&pkg, &["files", "ls", "--store", store.to_str().unwrap()]));
    let entries = data(&ls)["entries"].as_array().unwrap();
    let by_path: std::collections::BTreeMap<&str, &serde_json::Value> =
        entries.iter().map(|e| (e["path"].as_str().unwrap(), e)).collect();

    assert!(by_path.contains_key("README.md"), "README.md missing from manifest: {entries:?}");
    assert_eq!(by_path["README.md"]["mode"], "100644");
    assert!(by_path.contains_key("tests/smoke.lex"), "tests/smoke.lex missing: {entries:?}");
    assert!(by_path.contains_key("logo.bin"), "logo.bin missing: {entries:?}");
    assert_eq!(by_path["logo.bin"]["size"].as_u64(), Some(8));
    assert!(by_path.contains_key("run.sh"), "run.sh missing: {entries:?}");
    #[cfg(unix)]
    assert_eq!(by_path["run.sh"]["mode"], "100755", "exec bit not captured");

    // `lex files cat` round-trips the binary's exact bytes.
    let cat = json(&ok(&pkg, &["files", "cat", "--store", store.to_str().unwrap(), "logo.bin"]));
    use base64::Engine as _;
    let b64 = data(&cat)["content_b64"].as_str().expect("content_b64");
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
    assert_eq!(bytes, std::fs::read(&bin).unwrap());
}

// ── idempotency: unchanged republish is a true no-op ────────────────────────

#[test]
fn unchanged_directory_republish_creates_zero_ops_including_no_setfiles() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "idempotent");
    write(&pkg, "README.md", b"hello\n");
    let store = store_of(&pkg);

    ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let second = ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let v = json(&second);
    let ops = ops_of(&v);
    assert!(ops.is_empty(), "unchanged republish must apply zero ops (incl. no SetFiles), got {ops:?}");
}

// ── files-only edit: exactly one SetFiles, zero semantic ops ───────────────

#[test]
fn files_only_edit_creates_exactly_one_setfiles_op_and_zero_semantic_ops() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "readmeonly");
    write(&pkg, "README.md", b"v1\n");
    let store = store_of(&pkg);

    ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);

    // Touch only the README -- no source change.
    write(&pkg, "README.md", b"v2\n");
    let second = ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let v = json(&second);
    let kinds = op_kinds(&v);
    assert_eq!(kinds, vec!["set_files".to_string()], "expected exactly one SetFiles op, got {kinds:?}");
}

// ── reserved src/**/*.lex never in the manifest, even git-tracked there ────
// ── gitignored excluded, force-added included ───────────────────────────────

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(dir).args(args).output().expect("run git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn git_scan_excludes_reserved_and_gitignored_includes_force_added() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "gitscan");
    write(&pkg, ".gitignore", b"*.secret\nignored_dir/\n");
    write(&pkg, "secret.txt.secret", b"do not capture me\n");
    let keep_log = write(&pkg, "keep.secret", b"force-added despite matching *.secret\n");
    write(&pkg, "README.md", b"tracked normally\n");

    git(&pkg, &["init", "-q"]);
    git(&pkg, &["add", "lex.toml", "src/a.lex", "README.md", ".gitignore"]);
    // Force-add a file that matches .gitignore -- git tracks it regardless.
    git(&pkg, &["add", "-f", keep_log.file_name().unwrap().to_str().unwrap()]);

    let store = store_of(&pkg);
    ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let ls = json(&ok(&pkg, &["files", "ls", "--store", store.to_str().unwrap()]));
    let paths: Vec<&str> = data(&ls)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();

    assert!(paths.contains(&"README.md"), "README.md should be captured: {paths:?}");
    assert!(paths.contains(&"keep.secret"), "a force-added, otherwise-gitignored file must be captured: {paths:?}");
    assert!(!paths.contains(&"secret.txt.secret"), "an untracked gitignored file must be excluded: {paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("src/") && p.ends_with(".lex")),
        "src/**/*.lex is op-log-owned and must never appear in the files manifest, got {paths:?}"
    );
    // lex.toml itself is NOT reserved (only src/**/*.lex and src.lex are) --
    // it is captured verbatim as a blob, same as any other manifest path.
    assert!(paths.contains(&"lex.toml"), "lex.toml should be captured: {paths:?}");
}

// ── §0 fix: a no-op republish must not rewrite the head's committed lock ────

#[test]
fn no_op_republish_leaves_local_committed_lock_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "lockpkg");
    write(
        &pkg,
        "lex.lock",
        b"version = 1\n\n[[package]]\nname = \"dep\"\nversion = \"0.1.0\"\nhead_op = \"op_v1\"\n",
    );
    let store = store_of(&pkg);

    let first = ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let head1 = data(&json(&first))["head_op"].as_str().expect("head_op").to_string();

    let lock1 = lex_store::Store::open(&store)
        .unwrap()
        .committed_lock(&head1)
        .unwrap()
        .expect("lock committed on the first publish");
    assert!(lock1.contains("op_v1"));

    // A true no-op: nothing on disk changed at all.
    let second = ok(&pkg, &["publish", "--store", store.to_str().unwrap(), "."]);
    let v2 = json(&second);
    assert!(ops_of(&v2).is_empty(), "expected zero ops on an unchanged republish");
    let head2 = data(&v2)["head_op"].as_str().expect("head_op").to_string();
    assert_eq!(head1, head2, "head must not move on a true no-op republish");

    let lock2 = lex_store::Store::open(&store)
        .unwrap()
        .committed_lock(&head1)
        .unwrap()
        .expect("lock must still be there");
    assert_eq!(
        lock1, lock2,
        "a no-op republish must not rewrite the head's committed lock (#1007 §0)"
    );
}

// ── `lex files status|ls|cat|checkout` round-trip ───────────────────────────

#[test]
fn files_status_ls_cat_checkout_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    scaffold(&pkg, "roundtrip");
    write(&pkg, "README.md", b"hello world\n");
    let store = store_of(&pkg);
    let store_s = store.to_str().unwrap();

    // status before any publish: README + lex.toml read as added; src never.
    let st0 = json(&ok(&pkg, &["files", "status", "--store", store_s, "."]));
    let added0: Vec<String> = data(&st0)["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(added0.contains(&"README.md".to_string()), "{added0:?}");
    assert!(added0.contains(&"lex.toml".to_string()), "{added0:?}");
    assert!(!added0.iter().any(|p| p.starts_with("src/")), "{added0:?}");

    ok(&pkg, &["publish", "--store", store_s, "--activate", "."]);

    // status after a matching publish: clean.
    let st1 = json(&ok(&pkg, &["files", "status", "--store", store_s, "."]));
    assert!(data(&st1)["added"].as_array().unwrap().is_empty());
    assert!(data(&st1)["modified"].as_array().unwrap().is_empty());
    assert!(data(&st1)["deleted"].as_array().unwrap().is_empty());

    // ls
    let ls = json(&ok(&pkg, &["files", "ls", "--store", store_s]));
    let paths: Vec<&str> = data(&ls)["entries"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"README.md"));
    assert!(paths.contains(&"lex.toml"));

    // cat
    let cat = json(&ok(&pkg, &["files", "cat", "--store", store_s, "README.md"]));
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data(&cat)["content_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, b"hello world\n");

    // checkout -- src/ + manifest files materialize with no git involved.
    let out_dir = dir.path().join("checked_out");
    ok(&pkg, &["files", "checkout", "--store", store_s, out_dir.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(out_dir.join("README.md")).unwrap(), "hello world\n");
    assert_eq!(std::fs::read_to_string(out_dir.join("lex.toml")).unwrap(), std::fs::read_to_string(pkg.join("lex.toml")).unwrap());
    assert!(out_dir.join("src/a.lex").exists() || out_dir.join("src.lex").exists(), "src was not checked out");

    // Edit README only -> status sees it modified; commit records it.
    write(&pkg, "README.md", b"hello world v2\n");
    let st2 = json(&ok(&pkg, &["files", "status", "--store", store_s, "."]));
    let modified2: Vec<String> = data(&st2)["modified"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(modified2, vec!["README.md".to_string()]);

    let commit = json(&ok(&pkg, &["files", "commit", "--store", store_s, "-m", "update readme", "."]));
    let ops = ops_of(&commit);
    assert_eq!(ops.len(), 1, "commit should produce exactly one op: {ops:?}");
    assert_eq!(ops[0].pointer("/kind/op").and_then(|k| k.as_str()), Some("set_files"));

    let st3 = json(&ok(&pkg, &["files", "status", "--store", store_s, "."]));
    assert!(data(&st3)["modified"].as_array().unwrap().is_empty());

    // A second commit with nothing changed reports zero ops.
    let commit2 = json(&ok(&pkg, &["files", "commit", "--store", store_s, "."]));
    assert!(ops_of(&commit2).is_empty(), "no-op commit should record zero ops");
}
