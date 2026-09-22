//! A git dependency on a *moving* ref is cached by the commit it resolves to.
//!
//! The cache is consulted with a bare `pkg_dir.exists()`, so a name-keyed slot
//! is populated once and never revisited. An unpinned git dep
//! (`{ git = "…" }`, no rev) used to land in `~/.lex/packages/{name}` and stay
//! there:
//!
//! * `lex-economy` was cloned into that slot on one day; a rename upstream weeks
//!   later was invisible locally, and `lex check` reported `unknown_variant`
//!   against an interface that no longer existed
//! * the same stale directory made `bin/check-dep-drift.sh` — which compares
//!   the lock against the cache — report perfect agreement while CI failed
//! * and it nearly poisoned a 34-package registry migration, which type-checks
//!   each package against its dependencies
//!
//! Registry packages never had this problem: they are keyed `{name}-{version}`.
//! Keying a moving ref by its resolved commit gives git deps the same property —
//! a moved branch is simply a different directory, so staleness stops being
//! something to detect and becomes something that cannot happen.
//!
//! These tests drive a real local git repository, because the behaviour being
//! checked is precisely the interaction with `git ls-remote`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The cache root is read from a process-global env var, so these tests cannot
/// run concurrently: one would point the loader at another's cache and see an
/// empty directory. Serialising here rather than relying on `--test-threads=1`,
/// which nothing enforces at the call site.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn git(args: &[&str], cwd: &Path) {
    let st = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

/// A package repo whose default branch can be advanced between resolutions.
fn upstream(tmp: &Path, body: &str) -> PathBuf {
    let repo = tmp.join("upstream");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("lex.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\n").unwrap();
    std::fs::write(repo.join("src/lib.lex"), body).unwrap();
    git(&["init", "-q", "-b", "main"], &repo);
    git(&["add", "-A"], &repo);
    git(&["commit", "-q", "-m", "one"], &repo);
    repo
}

fn advance(repo: &Path, body: &str) {
    std::fs::write(repo.join("src/lib.lex"), body).unwrap();
    git(&["add", "-A"], repo);
    git(&["commit", "-q", "-m", "two"], repo);
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A consumer package declaring an unpinned git dep on `repo`.
fn consumer(tmp: &Path, repo: &Path) -> PathBuf {
    let dir = tmp.join("consumer");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("lex.toml"),
        format!(
            "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\n\n[dependencies]\ndep = {{ git = \"{}\" }}\n",
            repo.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/main.lex"), "import \"dep/lib\" as d\n\nfn go() -> Int { d.v() }\n").unwrap();
    dir
}

/// Resolve the dep through the real loader and report which cache directory it
/// used, plus whether the fetched source is the newest.
fn resolve(consumer_dir: &Path, cache: &Path) -> Vec<String> {
    // These tests pin the *strict* property — a moved branch is seen by the
    // very next load — so the shared cross-process ref cache (#1015
    // follow-up) is off unless a test turns it on with `resolve_with_ttl`.
    resolve_with_ttl(consumer_dir, cache, 0)
}

fn resolve_with_ttl(consumer_dir: &Path, cache: &Path, ttl_secs: u64) -> Vec<String> {
    std::env::set_var("LEX_GIT_REF_TTL", ttl_secs.to_string());
    // The loader reads the cache root from this env var.
    std::env::set_var("LEX_PACKAGES_DIR", cache);
    let entry = consumer_dir.join("src/main.lex");
    // Resolution happens as a side effect of loading the package; we only care
    // that the cache directory it chose encodes a commit.
    let _ = lex_syntax::loader::load_package(
        &[entry],
        consumer_dir,
        "consumer",
        // Inlining is what actually resolves a package dep through the cache;
        // with it off (#930) the import stays an unresolved edge and nothing is
        // fetched at all.
        true,
    );
    let mut dirs: Vec<String> = std::fs::read_dir(cache)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                // `.git-refs` is the shared ref cache, not a package.
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// An unpinned git dep must be cached under the commit it resolved to, not a
/// bare package name. A bare name is the slot that cannot be refreshed.
#[test]
fn an_unpinned_git_dep_is_cached_under_its_commit() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();

    let dirs = resolve(&cons, &cache);
    let sha = head_sha(&repo);
    assert!(
        dirs.iter().any(|d| d.contains("@rev-") && sha.starts_with(&d[d.find("@rev-").unwrap() + 5..])),
        "cache dir must encode the resolved commit {}, got {dirs:?}",
        &sha[..12]
    );
    assert!(
        !dirs.iter().any(|d| d == "dep"),
        "a bare name-keyed slot is the thing that cannot be refreshed: {dirs:?}"
    );
}

/// **The property that matters.** Advance the branch and resolve again: the
/// second resolution must use a *different* directory, so it cannot read the
/// first checkout. This is the exact failure `lex-economy` caused.
#[test]
fn moving_the_branch_changes_the_cache_directory() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();

    let before = resolve(&cons, &cache);
    let sha_before = head_sha(&repo);

    advance(&repo, "fn v() -> Int { 2 }\n");
    let after = resolve(&cons, &cache);
    let sha_after = head_sha(&repo);
    assert_ne!(sha_before, sha_after, "setup: the branch really moved");

    let new_dirs: Vec<&String> = after.iter().filter(|d| !before.contains(d)).collect();
    assert!(
        !new_dirs.is_empty(),
        "a moved branch must resolve to a new cache directory, not reuse {before:?}"
    );
    assert!(
        new_dirs.iter().any(|d| d.contains(&sha_after[..12])),
        "…and that directory must name the new commit {}, got {new_dirs:?}",
        &sha_after[..12]
    );
}

/// The newly-cached checkout must actually contain the new content — keying by
/// commit is pointless if the clone still fetches the old tree.
#[test]
fn the_new_directory_holds_the_new_source() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();

    resolve(&cons, &cache);
    advance(&repo, "fn v() -> Int { 2 }\n");
    resolve(&cons, &cache);

    let sha = head_sha(&repo);
    let dir = std::fs::read_dir(&cache)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains(&sha[..12]))
        .expect("a directory for the new commit");
    let src = std::fs::read_to_string(dir.path().join("src/lib.lex")).expect("read cached source");
    assert!(src.contains("2"), "the new checkout must hold the new source: {src}");
}

/// An explicitly pinned rev was never affected and must keep its existing
/// directory name, so pinned builds do not re-clone on upgrade.
#[test]
fn an_explicit_rev_keeps_its_existing_cache_name() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let sha = head_sha(&repo);
    let dir = tmp.path().join("pinned");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("lex.toml"),
        format!(
            "[package]\nname = \"c\"\nversion = \"0.1.0\"\n\n[dependencies]\ndep = {{ git = \"{}\", rev = \"{sha}\" }}\n",
            repo.display()
        ),
    )
    .unwrap();
    std::fs::write(dir.join("src/main.lex"), "import \"dep/lib\" as d\n\nfn go() -> Int { d.v() }\n").unwrap();

    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let dirs = resolve(&dir, &cache);
    assert!(
        dirs.iter().any(|d| d == &format!("dep@rev-{}", &sha[..12])),
        "a pinned rev keeps the name it already had, got {dirs:?}"
    );
}

/// #1015: resolution runs once per import *site*, so without a per-load memo
/// every site paid its own `git ls-remote` — a module importing a dep from a
/// dozen places asked the network a dozen times. One load, several sites,
/// one dep → exactly one `ls-remote`. A `git` shim on PATH counts them.
#[test]
fn one_load_asks_ls_remote_once_per_dependency() {
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\nfn w() -> Int { 2 }\n");
    let cons = consumer(tmp.path(), &repo);
    // Three import sites of the same dep across two files.
    std::fs::write(
        cons.join("src/main.lex"),
        "import \"dep/lib\" as d\nimport \"./other\" as o\n\nfn go() -> Int { d.v() + o.more() }\n",
    )
    .unwrap();
    std::fs::write(
        cons.join("src/other.lex"),
        "import \"dep/lib\" as d\nimport \"dep/lib\" as d2\n\nfn more() -> Int { d.w() + d2.v() }\n",
    )
    .unwrap();

    let cache = tmp.path().join("cache");
    let log = count_git(tmp.path(), || {
        resolve(&cons, &cache);
    });
    assert_eq!(ls_remotes(&log), 1, "one dependency, one ls-remote per load; git calls:\n{log}");
}

/// Run `f` with a `git` shim first on PATH that logs every invocation;
/// return the log.
fn count_git(tmp: &Path, f: impl FnOnce()) -> String {
    let real_git = String::from_utf8(
        Command::new("sh").args(["-c", "command -v git"]).output().expect("which git").stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let shim_dir = tmp.join("shim");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let log = tmp.join("git.log");
    let _ = std::fs::remove_file(&log);
    let shim = shim_dir.join("git");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n", log.display(), real_git),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old_path}", shim_dir.display()));
    f();
    std::env::set_var("PATH", old_path);
    std::fs::read_to_string(&log).unwrap_or_default()
}

fn ls_remotes(log: &str) -> usize {
    log.lines().filter(|l| l.starts_with("ls-remote")).count()
}

/// Across processes (#1015 follow-up): each `lex` process is its own load,
/// so a CI job running `lex check` per file paid one ls-remote per dep per
/// process. Within `LEX_GIT_REF_TTL`, a second load reuses the shared answer.
#[test]
fn a_second_load_within_the_ttl_reuses_the_resolved_ref() {
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    let log = count_git(tmp.path(), || {
        resolve_with_ttl(&cons, &cache, 60);
        resolve_with_ttl(&cons, &cache, 60);
    });
    assert_eq!(ls_remotes(&log), 1, "two loads within the TTL, one ls-remote:\n{log}");
}

/// `LEX_GIT_REF_TTL=0` turns the shared cache off: every load asks.
#[test]
fn ttl_zero_asks_every_load() {
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    let log = count_git(tmp.path(), || {
        resolve_with_ttl(&cons, &cache, 0);
        resolve_with_ttl(&cons, &cache, 0);
    });
    assert_eq!(ls_remotes(&log), 2, "TTL 0 must not share answers:\n{log}");
    assert!(!cache.join(".git-refs").exists(), "TTL 0 must not write the shared cache");
}

/// The bound on freshness: within the TTL a moved branch is not yet seen;
/// once the answer is older than the TTL, the next load sees the new commit.
#[test]
fn a_moved_branch_is_seen_once_the_ttl_expires() {
    let _g = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = upstream(tmp.path(), "fn v() -> Int { 1 }\n");
    let cons = consumer(tmp.path(), &repo);
    let cache = tmp.path().join("cache");
    let first = head_sha(&repo);
    let before = resolve_with_ttl(&cons, &cache, 3);
    advance(&repo, "fn v() -> Int { 2 }\n");
    let within = resolve_with_ttl(&cons, &cache, 3);
    assert_eq!(before, within, "inside the TTL the shared answer stands");
    assert!(before.iter().any(|d| d.contains(&first[..12])), "{before:?}");
    std::thread::sleep(std::time::Duration::from_millis(3200));
    let second = head_sha(&repo);
    let after = resolve_with_ttl(&cons, &cache, 3);
    assert!(
        after.iter().any(|d| d.contains(&second[..12])),
        "after the TTL the moved branch must resolve to its new commit: {after:?}"
    );
}
