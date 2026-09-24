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

// ── #1007 PR 6: manifest-aware export ────────────────────────────────────
//
// A directory publish captures non-op-log files by default (PR 4), which
// lands as one `SetFiles` op alongside the semantic ops (PR 2). These
// tests exercise the export renderer's side of that: a running manifest
// during the walk, materialized beside the op-log's `src/**/*.lex` tree.

/// A directory publish, capturing files (the PR 4 default — no
/// `--no-files`). Returns the store path.
fn publish_dir(store: &std::path::Path, pkg_dir: &std::path::Path) -> std::path::PathBuf {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json", "publish", "--activate",
            "--store", store.to_str().unwrap(),
            pkg_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
    store.to_path_buf()
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o111 != 0
}

/// README differing across two commits (§7 must-pass list): two directory
/// publishes with different README contents must land as two distinct
/// `SetFiles` ops, and the exported commit for each publish state must
/// show that state's README — not the other one's.
#[test]
fn export_readme_differs_across_two_publish_states() {
    let pkg = tempdir().unwrap();
    let root = pkg.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"readmediff\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.lex"), "fn id(x :: Int) -> Int { x }\n").unwrap();
    std::fs::write(root.join("README.md"), "# v1\n").unwrap();

    let store_dir = tempdir().unwrap();
    publish_dir(store_dir.path(), root);

    // A files-only change: same source, new README -> exactly one more
    // `SetFiles` op, no semantic op.
    std::fs::write(root.join("README.md"), "# v2, edited\n").unwrap();
    publish_dir(store_dir.path(), root);

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store_dir.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    let shas: Vec<String> = git(out.path(), &["log", "--format=%H", "--reverse"])
        .lines().map(String::from).collect();
    // 3 ops total: the first publish's semantic op (no files yet) + its
    // `SetFiles`, then the second publish's `SetFiles` alone.
    assert!(shas.len() >= 3, "expected >=3 commits (semantic + 2 SetFiles), got {}", shas.len());

    let readme_at = |sha: &str| -> Option<String> {
        let o = Command::new("git").arg("-C").arg(out.path())
            .args(["show", &format!("{sha}:README.md")]).output().unwrap();
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
    };
    // README doesn't exist before the first SetFiles op lands -- find the
    // first commit that carries it at all, and check it's the v1 state
    // (not v2 leaking backward), then check the tip is v2.
    let first = shas.iter().find_map(|s| readme_at(s))
        .unwrap_or_else(|| panic!("no commit ever carries README.md"));
    let last = readme_at(shas.last().unwrap()).expect("tip commit must carry README.md");
    assert!(first.contains("v1"), "first README-bearing commit must be v1, got: {first}");
    assert!(last.contains("v2, edited"), "last commit's README must be v2, got: {last}");
    assert_ne!(first, last, "README must actually differ between the two publish states");

    // The files-only commit carries a `Files:` trailer naming its manifest.
    let logtext = git(out.path(), &["log", "--format=%B"]);
    assert!(logtext.contains("Files: "), "a SetFiles commit must carry a Files: trailer:\n{logtext}");
}

/// `100755` must survive rendering: a manifest-captured executable script
/// keeps its executable bit in the exported working tree.
#[test]
#[cfg(unix)]
fn export_preserves_the_executable_bit() {
    let pkg = tempdir().unwrap();
    let root = pkg.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"execbit\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("src/lib.lex"), "fn id(x :: Int) -> Int { x }\n").unwrap();
    let script = root.join("bin/run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let store_dir = tempdir().unwrap();
    publish_dir(store_dir.path(), root);

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store_dir.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    let exported = out.path().join("bin/run.sh");
    assert!(exported.exists(), "bin/run.sh must be exported");
    assert!(is_executable(&exported), "the 100755 mode must survive export");
}

/// The bug the design flagged: `export-git` wipes `out_dir/src` before
/// re-rendering the op-log's `.lex` tree on *every* commit. A manifest
/// path that happens to live under `src/` (only `src/**/*.lex` is
/// op-log-reserved — `src/data.txt` is a legal manifest entry) must
/// survive that wipe, including across a LATER commit that is a purely
/// semantic op (no `SetFiles` at all) and so never re-touches the
/// manifest on its own.
#[test]
fn export_survives_the_src_wipe_for_a_non_lex_file_nested_under_src() {
    let pkg = tempdir().unwrap();
    let root = pkg.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"srcnested\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.lex"), "fn id(x :: Int) -> Int { x }\n").unwrap();
    std::fs::write(root.join("src/data.txt"), "nested non-lex payload\n").unwrap();

    let store_dir = tempdir().unwrap();
    publish_dir(store_dir.path(), root);

    // A later, purely semantic publish (single-file, no directory scan):
    // this op carries NO manifest of its own, so the running manifest is
    // only inherited -- the wipe-and-rerender still happens, and
    // src/data.txt must still be there afterward.
    std::fs::write(
        root.join("src/lib.lex"),
        "fn id(x :: Int) -> Int { x }\nfn twice(x :: Int) -> Int { x * 2 }\n",
    ).unwrap();
    let out2 = Command::new(lex_bin())
        .args([
            "--output", "json", "publish", "--store", store_dir.path().to_str().unwrap(),
            root.join("src/lib.lex").to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out2.status.success(), "second publish: {}", String::from_utf8_lossy(&out2.stderr));

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store_dir.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    // Final working tree, after the semantic-only commit, must still have
    // the nested manifest file.
    let nested = out.path().join("src/data.txt");
    assert!(nested.exists(), "src/data.txt must survive the src/ wipe on a later semantic-only commit");
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "nested non-lex payload\n");

    // And it must have been present at every commit from the point the
    // manifest first captured it onward -- in particular at the tip, the
    // later purely-semantic commit that never touches the manifest at all
    // (that's the exact bug the design flagged: a naive wipe would drop it
    // there even though nothing about the files changed). The very first
    // commit legitimately predates any `SetFiles` op, so it's excluded.
    let shas: Vec<String> = git(out.path(), &["log", "--format=%H", "--reverse"])
        .lines().map(String::from).collect();
    assert!(shas.len() >= 3, "expected >=3 commits, got {}", shas.len());
    for sha in &shas[1..] {
        let has = Command::new("git").arg("-C").arg(out.path())
            .args(["cat-file", "-e", &format!("{sha}:src/data.txt")])
            .status().unwrap();
        assert!(has.success(), "commit {sha} must carry src/data.txt in its tree");
    }
}

/// Backward-compat regression (critical): a store with NO `SetFiles` ops
/// at all (the pre-#1007 shape -- every existing test above this one in
/// this file publishes single files, never a directory) must export with
/// zero manifest-related side effects: no extra files beyond the rendered
/// `.lex` source, and no `Files:` trailer anywhere in the log. If this
/// regresses, PR 6 broke every pre-#1007 store's export.
#[test]
fn export_of_a_store_with_no_set_files_ops_is_unaffected() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn a(x :: Int) -> Int { x }\n");
    publish(store.path(), "fn a(x :: Int) -> Int { x }\nfn b(x :: Int) -> Int { x + 1 }\n");

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));

    // Nothing beyond .git/ and src.lex was ever written to out_dir.
    let entries: Vec<String> = std::fs::read_dir(out.path()).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".git")
        .collect();
    assert_eq!(entries, vec!["src.lex".to_string()],
        "a SetFiles-free store must export exactly src.lex beside .git/, got {entries:?}");

    let logtext = git(out.path(), &["log", "--format=%B"]);
    assert!(!logtext.contains("Files: "),
        "a SetFiles-free store's commits must never carry a Files: trailer:\n{logtext}");
}
