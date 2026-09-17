//! `lex export-git` — render a branch's op history as a git repo (#837).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, text: &str) {
    let src = store.join("a.lex");
    std::fs::write(&src, text).unwrap();
    let out = Command::new(lex_bin())
        .args(["--output", "json", "publish", "--store", store.to_str().unwrap(), src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn export_produces_a_commit_per_op_and_the_head_source() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    publish(store.path(), "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn mul(x :: Int, y :: Int) -> Int { x * y }\n");

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["--output", "json", "export-git", out.path().to_str().unwrap(),
               "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));
    let v: serde_json::Value = serde_json::from_slice(&res.stdout).unwrap();
    let commits = v.pointer("/data/commits").unwrap().as_u64().unwrap();
    assert!(commits >= 2, "expected >=2 commits (one per op), got {commits}");

    // The repo is real and its log has that many commits.
    let count: u64 = git(out.path(), &["rev-list", "--count", "HEAD"]).trim().parse().unwrap();
    assert_eq!(count, commits, "git log commit count must match reported commits");

    // The working tree's head source contains both functions.
    let src = std::fs::read_to_string(out.path().join("src.lex")).unwrap();
    assert!(src.contains("fn add"), "head source must contain add: {src}");
    assert!(src.contains("fn mul"), "head source must contain mul: {src}");

    // Commit messages carry the op trailer (traceable back to the log).
    let logtext = git(out.path(), &["log", "--format=%B"]);
    assert!(logtext.contains("Op: "), "commit messages must carry an Op: trailer");
}

#[test]
fn export_is_deterministic_and_re_runnable() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn id(x :: Int) -> Int { x }\n");
    let out = tempdir().unwrap();
    let run = || Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(run().status.success());
    // Re-running into the same dir must not error (idempotent tool).
    assert!(run().status.success(), "re-export must succeed");
}

/// #895: a module's `import`s (with their real aliases) and `type`
/// declarations must survive the op-log round-trip, so the exported
/// source is a compilable module — not just its function bodies.
#[test]
fn export_round_trips_imports_and_types_to_compilable_source() {
    let store = tempdir().unwrap();
    // Non-default alias (`as integer`, not the default `int`) exercises
    // alias capture; the `type` exercises type capture; `show` uses both.
    let source = concat!(
        "import \"std.int\" as integer\n",
        "type Wrapped = { n :: Int }\n",
        "fn show(w :: Wrapped) -> Str { integer.to_str(w.n) }\n",
    );
    publish(store.path(), source);

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(),
               "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    let rendered = std::fs::read_to_string(out.path().join("src.lex")).unwrap();
    assert!(rendered.contains("import \"std.int\" as integer"),
        "the non-default alias must round-trip verbatim, got:\n{rendered}");
    assert!(rendered.contains("type Wrapped"),
        "the type declaration must round-trip, got:\n{rendered}");

    // The decisive check: the rendered module type-checks. Before #895
    // it did not — imports and types were dropped, so `integer` and
    // `Wrapped` were unresolved.
    let check = Command::new(lex_bin())
        .args(["check", out.path().join("src.lex").to_str().unwrap()])
        .output().unwrap();
    assert!(check.status.success(),
        "rendered module must type-check: {}", String::from_utf8_lossy(&check.stderr));
}

/// #894 slice 2b: `export-git` de-flattens a multi-module package back
/// into its `src/*.lex` tree, each file type-checking. Also pins the
/// alias-collision case: `main` imports the `util` module while taking a
/// param literally named `util`, so the naive stem alias would be
/// shadowed — the de-flatten must pick a non-colliding alias.
#[test]
fn export_deflattens_a_multi_module_package() {
    let store_dir = tempdir().unwrap();
    let pkg = tempdir().unwrap();
    let root = pkg.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n").unwrap();
    let src = root.join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("util.lex"), "fn helper(x :: Int) -> Int { x + 1 }\n").unwrap();
    // `util` param name collides with the `util` module's stem alias.
    std::fs::write(
        src.join("main.lex"),
        "import \"./util\" as u\nfn run(util :: Int) -> Int { u.helper(util) }\n",
    )
    .unwrap();
    let store = store_dir.path();

    let pubout = Command::new(lex_bin())
        .args([
            "--output", "json", "publish", "--activate",
            "--store", store.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(pubout.status.success(), "publish: {}", String::from_utf8_lossy(&pubout.stderr));

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    // The multi-file tree is reconstructed, not a single src.lex.
    let util = out.path().join("src/util.lex");
    let main = out.path().join("src/main.lex");
    assert!(util.exists() && main.exists(), "expected src/util.lex and src/main.lex");
    assert!(!out.path().join("src.lex").exists(), "should be a tree, not one src.lex");

    // Both files type-check as a package.
    std::fs::copy(root.join("lex.toml"), out.path().join("lex.toml")).unwrap();
    for f in [&util, &main] {
        let check = Command::new(lex_bin())
            .args(["check", f.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(check.status.success(),
            "reconstructed {} must type-check: {}", f.display(), String::from_utf8_lossy(&check.stderr));
    }
}
