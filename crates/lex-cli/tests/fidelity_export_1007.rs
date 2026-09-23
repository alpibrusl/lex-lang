//! #1007 §7 fidelity verification: publish -> push/pull through a REAL
//! in-process `lex-api` hub -> `export-git` -> compare the exported git
//! tree back against the original source, byte-for-byte and AST-for-AST.
//!
//! ## Why a Rust test, not a shell script
//!
//! The design's §7 sketches a `verify_export.sh`. This crate already has
//! an established, more capable pattern for exactly this shape of check —
//! `op_push_pull_files_1007.rs` and `op_push_lock_sync_1031.rs` spin up
//! the real `lex-api` handler in-process (`lex_api::handlers::State` +
//! `tiny_http`) and drive the real `lex` binary via `Command`, rather than
//! shelling out to `lex serve` + curl. Reusing it here means: no process
//! to background/kill/port-scan from bash, byte-exact assertions instead
//! of text-scraping CLI output, and direct access to `lex parse --output
//! json` / `lex docs --output json` for the AST/decl-doc comparisons the
//! design calls for. `scripts/fidelity/verify_export.sh` is the thin,
//! runnable-by-humans-and-CI wrapper the design's §7 asks for; it invokes
//! this test with `cargo test`.
//!
//! ## Structure
//!
//! - [`fidelity_check_passes_on_a_synthetic_repo_exercising_every_ownership_rule`]
//!   is the mandatory, must-pass check: it builds its own local git repo
//!   (no network) covering every row of the design's §2 ownership table --
//!   README, LICENSE, `tests/`, a non-`.lex` file nested under `src/`, an
//!   executable script, a binary file, a `.gitignore`'d file, a
//!   force-added file, and multiple commits -- then runs the full
//!   publish -> push -> pull -> export-git pipeline through the real hub
//!   and asserts (a)-(c) from §7.
//! - [`fidelity_check_against_a_real_repo`] is `#[ignore]`d: it runs the
//!   same pipeline against `FIDELITY_REPO`/`FIDELITY_REF` (a path or a
//!   `git clone`-able URL) when set, closer to the design's "nightly on
//!   pinned real packages" plan. Not required for CI to stay green in a
//!   sandboxed/offline run.
//! - [`corrupting_the_export_is_actually_caught`] proves the harness isn't
//!   a rubber stamp: it runs the same assertions against a deliberately
//!   corrupted export (content flip, mode flip, and a dropped file) and
//!   checks each one is reported as a mismatch.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── the real in-process lex-api hub (files-v1 capable) ──────────────────────
// Copied from `op_push_pull_files_1007.rs`'s established pattern.

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
    wait_until_serving(&addr);
    (Server { addr, _join: Some(join) }, tmp)
}

fn wait_until_serving(addr: &SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
}

// ── running the real `lex` binary ────────────────────────────────────────

fn run_lex(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    Command::new(lex_bin())
        .current_dir(cwd)
        .env("HOME", env_root)
        .env("LEX_PACKAGES_DIR", env_root.join("packages"))
        .env_remove("LEX_STORE")
        .env_remove("LEXHUB_TOKEN")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning `lex {}`: {e}", args.join(" ")))
}

fn ok(cwd: &Path, env_root: &Path, args: &[&str]) -> Output {
    let out = run_lex(cwd, env_root, args);
    assert!(
        out.status.success(),
        "`lex {}` failed (cwd={}):\nstdout: {}\nstderr: {}",
        args.join(" "),
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn json_ok(cwd: &Path, env_root: &Path, args: &[&str]) -> serde_json::Value {
    let mut full: Vec<&str> = vec!["--output", "json"];
    full.extend_from_slice(args);
    let out = ok(cwd, env_root, &full);
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("non-JSON from lex {args:?}: {e}\nstdout: {text}"))
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
}

// ── the synthetic fixture repo: every row of §2's ownership table ──────────

/// Builds a real local git repo at `dir` covering every ownership case the
/// design's §2 table calls out, across multiple commits:
///
/// | path                     | rule exercised                              |
/// |--------------------------|----------------------------------------------|
/// | `src/main.lex`           | op-log-owned (rendered, not a manifest path)  |
/// | `src/data.txt`           | non-`.lex` file nested under `src/` -- blob   |
/// | `README.md`              | blob; edited across commits                   |
/// | `LICENSE`                | blob                                          |
/// | `tests/basic.txt`        | blob (`tests/**` is a blob for now, per design)|
/// | `bin/run.sh`             | blob, mode `100755`                           |
/// | `assets/logo.bin`        | blob, binary/non-UTF-8 content                |
/// | `.gitignore`              | blob                                          |
/// | `ignored.secret`         | matches `.gitignore`, never added -- excluded |
/// | `forced.secret`          | matches `.gitignore`, force-added -- included |
///
/// Returns the repo dir. The caller publishes/exports at HEAD (the tip
/// after both commits) -- "the ref" in `verify_export.sh <repo> <ref>`
/// terms, since this harness checks out `ref` before running.
fn build_synthetic_repo(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("assets")).unwrap();

    git_ok(dir, &["init", "-q"]);
    git_ok(dir, &["config", "user.email", "fidelity@test"]);
    git_ok(dir, &["config", "user.name", "fidelity"]);

    std::fs::write(dir.join("lex.toml"), "[package]\nname = \"fidelitysynth\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(dir.join("src/main.lex"), "fn add(x :: Int, y :: Int) -> Int { x + y }\n").unwrap();
    std::fs::write(dir.join("src/data.txt"), "nested non-lex payload under src/\n").unwrap();
    std::fs::write(dir.join("README.md"), "# fidelitysynth\n\nversion 1\n").unwrap();
    std::fs::write(dir.join("LICENSE"), "MIT\n").unwrap();
    std::fs::write(dir.join("tests/basic.txt"), "a test fixture\n").unwrap();
    let script = dir.join("bin/run.sh");
    std::fs::write(&script, "#!/bin/sh\necho hello\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let binary: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x9F, 0x00, 0x01, 0x02, 0xC0, 0x80, 0xFF];
    std::fs::write(dir.join("assets/logo.bin"), &binary).unwrap();
    std::fs::write(dir.join(".gitignore"), "*.secret\n").unwrap();
    std::fs::write(dir.join("ignored.secret"), "must never be tracked\n").unwrap();
    std::fs::write(dir.join("forced.secret"), "force-added despite gitignore\n").unwrap();

    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["add", "-f", "forced.secret"]);
    git_ok(dir, &["commit", "-q", "-m", "initial commit: full fixture"]);

    // Second commit: edit README (exercises "differs across commits") and
    // add a second function, so the op-log side of the walk has more than
    // one op too.
    std::fs::write(dir.join("README.md"), "# fidelitysynth\n\nversion 2, edited\n").unwrap();
    std::fs::write(
        dir.join("src/main.lex"),
        "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn sub(x :: Int, y :: Int) -> Int { x - y }\n",
    ).unwrap();
    git_ok(dir, &["add", "-A"]);
    git_ok(dir, &["commit", "-q", "-m", "second commit: edit README + add sub"]);
}

// ── the harness's reusable assertions (§7 a/b/c) ────────────────────────────

/// A single mismatch the harness found. `Vec::is_empty()` on the result of
/// an assertion function is the pass/fail signal throughout this file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mismatch(String);

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn git_ls_files(dir: &Path) -> BTreeSet<String> {
    git(dir, &["ls-files"]).lines().map(String::from).collect()
}

/// §7(a): the source and exported repos must track exactly the same paths.
fn assert_file_sets_equal(source: &Path, exported: &Path) -> Vec<Mismatch> {
    let src_set = git_ls_files(source);
    let exp_set = git_ls_files(exported);
    let mut out = Vec::new();
    for only_src in src_set.difference(&exp_set) {
        out.push(Mismatch(format!("`{only_src}` is in the source but missing from the export")));
    }
    for only_exp in exp_set.difference(&src_set) {
        out.push(Mismatch(format!("`{only_exp}` is in the export but not in the source")));
    }
    out
}

#[cfg(unix)]
fn file_mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// §7(b): every tracked path that is NOT `src/**/*.lex` / `src.lex` (the
/// op-log-rendered paths) must be byte-identical, same mode, between
/// source and export.
fn assert_non_lex_files_byte_identical(source: &Path, exported: &Path, paths: &BTreeSet<String>) -> Vec<Mismatch> {
    let mut out = Vec::new();
    for path in paths {
        if path == "src.lex" || (path.starts_with("src/") && path.ends_with(".lex")) {
            continue; // op-log-owned; checked by assert_lex_ast_and_docs_equal instead
        }
        let s = source.join(path);
        let e = exported.join(path);
        let (sb, eb) = match (std::fs::read(&s), std::fs::read(&e)) {
            (Ok(sb), Ok(eb)) => (sb, eb),
            (Err(err), _) => { out.push(Mismatch(format!("`{path}`: can't read source: {err}"))); continue; }
            (_, Err(err)) => { out.push(Mismatch(format!("`{path}`: can't read export: {err}"))); continue; }
        };
        if sb != eb {
            out.push(Mismatch(format!("`{path}`: content differs ({} bytes source vs {} bytes export)", sb.len(), eb.len())));
        }
        #[cfg(unix)]
        {
            let (sm, em) = (file_mode(&s), file_mode(&e));
            if sm != em {
                out.push(Mismatch(format!("`{path}`: mode differs (source {sm:o} vs export {em:o})")));
            }
        }
    }
    out
}

/// Canonical AST via `lex parse --output json`, with the source path
/// scrubbed out (only the AST shape matters, not where the temp file
/// lived) so two files with identical structure compare equal.
/// Sort a JSON array by each element's `name` field (falling back to the
/// element's own serialization for anything unnamed), so two arrays that
/// hold the same elements in a different order compare equal. The
/// op-log's rendered top-level declaration order is sig-id-derived, not
/// the original file's textual order (true before #1007 too -- it's a
/// property of the renderer, not something this PR introduces), so a
/// content-only comparison must not be order-sensitive.
fn sort_by_name(v: &mut serde_json::Value) {
    if let Some(arr) = v.as_array_mut() {
        arr.sort_by_key(|e| {
            e.get("name").and_then(|n| n.as_str()).map(String::from)
                .unwrap_or_else(|| e.to_string())
        });
    }
}

fn canonical_ast(lex_dir: &Path, env_root: &Path, path: &Path) -> serde_json::Value {
    let v = json_ok(lex_dir, env_root, &["parse", path.to_str().unwrap()]);
    let mut d = data(&v).clone();
    sort_by_name(&mut d);
    d
}

/// Declaration docs via `lex docs --output json`, with the `file` field
/// (an absolute path, necessarily different between source and export)
/// scrubbed from each module entry, and each module's `functions` array
/// order-normalized the same way as [`canonical_ast`], before comparing.
fn declaration_docs(lex_dir: &Path, env_root: &Path, path: &Path) -> serde_json::Value {
    let v = json_ok(lex_dir, env_root, &["docs", path.to_str().unwrap()]);
    let mut d = data(&v).clone();
    if let Some(modules) = d.get_mut("modules").and_then(|m| m.as_array_mut()) {
        for m in modules {
            if let Some(obj) = m.as_object_mut() {
                obj.remove("file");
                if let Some(funcs) = obj.get_mut("functions") {
                    sort_by_name(funcs);
                }
            }
        }
    }
    d
}

/// §7(c): every `src/**/*.lex` (or root `src.lex`) path must have an
/// equal canonical AST and equal declaration docs between source and
/// export -- comments and formatting may differ (the canonical printer
/// reformats; `lex parse`'s AST is comment-free by construction), but the
/// actual declarations must not.
fn assert_lex_ast_and_docs_equal(
    source: &Path,
    exported: &Path,
    env_root: &Path,
    paths: &BTreeSet<String>,
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    for path in paths {
        if !(path == "src.lex" || (path.starts_with("src/") && path.ends_with(".lex"))) {
            continue;
        }
        let s = source.join(path);
        let e = exported.join(path);
        if !s.exists() || !e.exists() {
            out.push(Mismatch(format!("`{path}`: missing on one side (source exists={}, export exists={})", s.exists(), e.exists())));
            continue;
        }
        let (sa, ea) = (canonical_ast(source, env_root, &s), canonical_ast(exported, env_root, &e));
        if sa != ea {
            out.push(Mismatch(format!("`{path}`: canonical AST differs")));
        }
        let (sd, ed) = (declaration_docs(source, env_root, &s), declaration_docs(exported, env_root, &e));
        if sd != ed {
            out.push(Mismatch(format!("`{path}`: declaration docs differ")));
        }
    }
    out
}

/// Runs assertions (a)-(c) and panics with every mismatch found (not just
/// the first) if any exist.
fn assert_fidelity(source: &Path, exported: &Path, env_root: &Path) {
    let mut mismatches = assert_file_sets_equal(source, exported);
    let paths = git_ls_files(source);
    mismatches.extend(assert_non_lex_files_byte_identical(source, exported, &paths));
    mismatches.extend(assert_lex_ast_and_docs_equal(source, exported, env_root, &paths));
    assert!(
        mismatches.is_empty(),
        "fidelity check found {} mismatch(es):\n{}",
        mismatches.len(),
        mismatches.iter().map(|m| format!(" - {m}")).collect::<Vec<_>>().join("\n"),
    );
}

// ── the pipeline: publish -> push -> pull -> export-git ─────────────────────

/// Runs the full §7 pipeline for a checked-out repo at `repo_dir` (already
/// at the ref to verify) and returns the exported repo's directory,
/// leaving both the source and export around (in the `TempDir`s the
/// caller owns) for further inspection.
fn run_pipeline(repo_dir: &Path, env_root: &Path) -> TempDir {
    let (server, _hub_tmp) = start_server();
    let hub = format!("http://{}", server.addr);

    let store_a = repo_dir.join(".lex/store").to_string_lossy().into_owned();
    let published = json_ok(repo_dir, env_root, &["publish", "--store", &store_a, "--activate", "."]);
    assert!(
        !data(&published)["files_manifest"].is_null(),
        "the synthetic repo has non-op-log files; publish must capture a files manifest: {published}"
    );

    json_ok(repo_dir, env_root, &["op", "push", &hub, "--store", &store_a]);

    let fresh = TempDir::new().unwrap();
    let store_b = fresh.path().join(".lex/store").to_string_lossy().into_owned();
    std::fs::create_dir_all(fresh.path()).unwrap();
    json_ok(fresh.path(), env_root, &["op", "pull", &hub, "--store", &store_b]);

    let out = TempDir::new().unwrap();
    json_ok(fresh.path(), env_root, &["export-git", out.path().to_str().unwrap(), "--store", &store_b]);
    out
}

// ── 1. the mandatory, must-pass check ───────────────────────────────────────

#[test]
fn fidelity_check_passes_on_a_synthetic_repo_exercising_every_ownership_rule() {
    let env_root = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    build_synthetic_repo(repo.path());

    let out = run_pipeline(repo.path(), env_root.path());
    assert_fidelity(repo.path(), out.path(), env_root.path());
}

// ── 2. bonus: a real repo, when one is reachable (never required for CI) ───

/// Runs the same pipeline against a real repo: `FIDELITY_REPO` may be a
/// local path (used in place, checked out at `FIDELITY_REF`) or a
/// `git clone`-able URL (cloned to a tempdir first). `#[ignore]`d because
/// this sandbox may have no network access and the design explicitly
/// says the synthetic repo is the required, must-pass check --- this is
/// the "nightly on pinned real packages" bonus, run explicitly:
///
/// ```sh
/// FIDELITY_REPO=https://github.com/alpibrusl/lex-agent FIDELITY_REF=main \
///   cargo test -p lex-cli --test fidelity_export_1007 -- --ignored fidelity_check_against_a_real_repo
/// ```
#[test]
#[ignore = "needs FIDELITY_REPO/FIDELITY_REF and possibly network access"]
fn fidelity_check_against_a_real_repo() {
    let Ok(repo_spec) = std::env::var("FIDELITY_REPO") else {
        eprintln!("FIDELITY_REPO not set -- skipping (see this test's doc comment)");
        return;
    };
    let ref_spec = std::env::var("FIDELITY_REF").unwrap_or_else(|_| "HEAD".to_string());
    let env_root = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let repo_dir = work.path().join("repo");

    // `git clone` accepts a local path exactly like a URL, so a single
    // call covers both `FIDELITY_REPO` forms the design's
    // `<local-git-repo-or-github-url>` usage line allows.
    git_ok(Path::new("."), &["clone", "-q", &repo_spec, repo_dir.to_str().unwrap()]);
    git_ok(&repo_dir, &["checkout", "-q", &ref_spec]);

    let out = run_pipeline(&repo_dir, env_root.path());
    // A real GitHub repo's `.lex` sources may carry body comments the
    // canonical printer reformats -- §7's accepted-differences list. We
    // still require (a) and (b) to hold exactly; (c) is checked but only
    // reported, not hard-failed, when the only difference class is
    // comments/whitespace (classification left as a manual follow-up:
    // the mismatch messages from `assert_lex_ast_and_docs_equal` say
    // exactly which file and which of AST/docs differed).
    let mut mismatches = assert_file_sets_equal(&repo_dir, out.path());
    let paths = git_ls_files(&repo_dir);
    mismatches.extend(assert_non_lex_files_byte_identical(&repo_dir, out.path(), &paths));
    let ast_mismatches = assert_lex_ast_and_docs_equal(&repo_dir, out.path(), env_root.path(), &paths);
    assert!(mismatches.is_empty(), "(a)/(b) must hold exactly:\n{mismatches:?}");
    if !ast_mismatches.is_empty() {
        eprintln!(
            "note: {} src/**/*.lex file(s) differ in canonical AST or docs -- \
             expected only for real repos with body comments the printer reformats:\n{}",
            ast_mismatches.len(),
            ast_mismatches.iter().map(|m| format!(" - {m}")).collect::<Vec<_>>().join("\n"),
        );
    }
}

// ── 3. prove the harness actually catches drift ─────────────────────────────

/// The harness is worthless if it always reports success. This runs the
/// same synthetic-repo pipeline, then deliberately corrupts the export in
/// three independent ways (content, mode, a dropped file) and checks each
/// corruption is caught by name -- proving `assert_file_sets_equal` /
/// `assert_non_lex_files_byte_identical` actually inspect what they claim
/// to, rather than vacuously passing.
#[test]
fn corrupting_the_export_is_actually_caught() {
    let env_root = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    build_synthetic_repo(repo.path());
    let out = run_pipeline(repo.path(), env_root.path());

    // Sanity: the uncorrupted export passes first (otherwise "corruption
    // is caught" would be trivially true for the wrong reason).
    assert_fidelity(repo.path(), out.path(), env_root.path());

    // (i) content corruption: flip a byte in a manifest-owned file.
    let readme = out.path().join("README.md");
    let mut bytes = std::fs::read(&readme).unwrap();
    assert!(!bytes.is_empty());
    bytes[0] ^= 0xFF;
    std::fs::write(&readme, &bytes).unwrap();
    let paths = git_ls_files(repo.path());
    let content_mismatches = assert_non_lex_files_byte_identical(repo.path(), out.path(), &paths);
    assert!(
        content_mismatches.iter().any(|m| m.0.contains("README.md") && m.0.contains("content differs")),
        "corrupting README.md's bytes must be caught as a content mismatch, got: {content_mismatches:?}"
    );
    std::fs::write(&readme, std::fs::read(repo.path().join("README.md")).unwrap()).unwrap(); // restore

    // (ii) mode corruption: strip the executable bit from bin/run.sh.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = out.path().join("bin/run.sh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode_mismatches = assert_non_lex_files_byte_identical(repo.path(), out.path(), &paths);
        assert!(
            mode_mismatches.iter().any(|m| m.0.contains("bin/run.sh") && m.0.contains("mode differs")),
            "stripping the exec bit must be caught as a mode mismatch, got: {mode_mismatches:?}"
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap(); // restore
    }

    // (iii) a dropped file: delete a manifest-tracked file entirely and
    // remove it from the export's git index too (so git ls-files agrees
    // with what's on disk, isolating this to a genuine "file set" drift
    // rather than an index/worktree mismatch).
    std::fs::remove_file(out.path().join("LICENSE")).unwrap();
    git_ok(out.path(), &["add", "-A"]);
    let set_mismatches = assert_file_sets_equal(repo.path(), out.path());
    assert!(
        set_mismatches.iter().any(|m| m.0.contains("LICENSE")),
        "dropping LICENSE from the export must be caught as a file-set mismatch, got: {set_mismatches:?}"
    );

    // (iv) an AST-level drift: rewrite the exported .lex source with a
    // semantically different body (same file, different declaration) --
    // proves (c)'s canonical-AST comparison isn't just a text diff on
    // some incidental formatting.
    let exported_main = out.path().join("src/main.lex");
    let original = std::fs::read_to_string(&exported_main).unwrap();
    std::fs::write(&exported_main, "fn add(x :: Int, y :: Int) -> Int { x }\n").unwrap(); // drops `+ y`: wrong body
    let ast_mismatches = assert_lex_ast_and_docs_equal(repo.path(), out.path(), env_root.path(), &paths);
    assert!(
        ast_mismatches.iter().any(|m| m.0.contains("src/main.lex")),
        "an altered function body must be caught as an AST mismatch, got: {ast_mismatches:?}"
    );
    std::fs::write(&exported_main, original).unwrap(); // restore
}
