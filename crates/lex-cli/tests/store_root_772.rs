//! #772: the store is scoped to the project by default.
//!
//! Without `--store`, every store-touching command used to fall back
//! to `$HOME/.lex/store`, so two unrelated projects on one machine
//! silently shared one branch namespace. Now the nearest `lex.toml`
//! above the working directory scopes the store (its `[store] path`,
//! else `.lex/store` beside it); `LEX_STORE` still wins; and only
//! outside any project does the global store apply, with a note on
//! stderr.

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

/// Run `lex` in `cwd` with HOME pointed at `home` and no `LEX_STORE`.
fn run_in(cwd: &Path, home: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(lex_bin());
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("LEX_STORE")
        .env("HOME", home);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn project(root: &Path, name: &str, manifest: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("lex.toml"), manifest).unwrap();
    std::fs::write(dir.join("f.lex"), "fn f() -> Int { 1 }\n").unwrap();
    dir
}

const MANIFEST: &str = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n";

#[test]
fn two_projects_do_not_share_a_store() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let a = project(tmp.path(), "a", MANIFEST);
    let b = project(tmp.path(), "b", MANIFEST);

    let (code, _, err) = run_in(&a, &home, &["branch", "create", "only-in-a"], &[]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!err.contains("shared global store"), "project store must not warn: {err}");

    let (_, out_a, _) = run_in(&a, &home, &["branch", "list"], &[]);
    let (_, out_b, _) = run_in(&b, &home, &["branch", "list"], &[]);
    assert!(out_a.contains("only-in-a"), "a: {out_a}");
    assert!(!out_b.contains("only-in-a"), "b must not see a's branch: {out_b}");

    assert!(a.join(".lex/store").is_dir(), "store lives beside a's lex.toml");
    assert!(!home.join(".lex/store").exists(), "the global store must be untouched");
}

#[test]
fn nested_directory_resolves_to_the_project_store() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let a = project(tmp.path(), "a", MANIFEST);
    let nested = a.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    let (code, _, err) = run_in(&nested, &home, &["branch", "create", "from-nested"], &[]);
    assert_eq!(code, 0, "stderr: {err}");
    let (_, out, _) = run_in(&a, &home, &["branch", "list"], &[]);
    assert!(out.contains("from-nested"), "{out}");
    assert!(!nested.join(".lex").exists());
}

#[test]
fn manifest_store_path_is_honoured() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let a = project(tmp.path(), "a", "[package]\nname = \"p\"\n\n[store]\npath = \"var/lexstore\"\n");

    let (code, _, err) = run_in(&a, &home, &["branch", "create", "custom"], &[]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(a.join("var/lexstore/branches").is_dir(), "declared path is used");
    assert!(!a.join(".lex").exists(), "default location is not used");
}

#[test]
fn lex_store_env_overrides_the_manifest() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let a = project(tmp.path(), "a", MANIFEST);
    let explicit = tmp.path().join("explicit");

    let (code, _, err) = run_in(
        &a,
        &home,
        &["branch", "create", "via-env"],
        &[("LEX_STORE", explicit.to_str().unwrap())],
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert!(explicit.join("branches").is_dir());
    assert!(!a.join(".lex").exists());
    assert!(!err.contains("shared global store"));
}

#[test]
fn outside_any_project_the_global_store_is_used_and_announced() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    let loose = tmp.path().join("loose");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&loose).unwrap();
    std::fs::write(loose.join("f.lex"), "fn f() -> Int { 1 }\n").unwrap();

    let (code, _, err) = run_in(&loose, &home, &["branch", "create", "global"], &[]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(home.join(".lex/store/branches").is_dir(), "global store is the fallback");
    assert!(err.contains("shared global store"), "must say so on stderr: {err}");
    assert!(err.contains(home.join(".lex/store").to_str().unwrap()), "names the path: {err}");

    // An explicit --store is silent.
    let (code, _, err) = run_in(&loose, &home, &["branch", "create", "explicit", "--store", "here"], &[]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!err.contains("shared global store"), "{err}");
}

#[test]
fn init_scaffolds_a_gitignore_for_the_project_store() {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let dir = tmp.path().join("fresh");
    let (code, out, err) = run_in(tmp.path(), &home, &["init", "fresh"], &[]);
    assert_eq!(code, 0, "stdout: {out}\nstderr: {err}");
    let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(gi.lines().any(|l| l == ".lex/store/"), "{gi}");
    let toml = std::fs::read_to_string(dir.join("lex.toml")).unwrap();
    assert!(toml.contains("[store]"), "{toml}");
}
