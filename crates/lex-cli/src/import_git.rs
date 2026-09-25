//! `lex op import-git` — the git → op-log importer (#892 PR 4 + PR 5).
//!
//! ```text
//! lex op import-git <path|url> [--branch B] [--store DIR] [--store-branch S]
//!                   [--head-only] [--on-error fold|stop] [--strict]
//!                   [--examples tip|all|none] [--max-file-bytes N]
//!                   [--depth N] [--since SHA] [--max-commits N]
//! ```
//!
//! Without `--head-only` this imports the FIRST-PARENT history of one branch,
//! one intent per commit; with it, the tip as one snapshot. Re-running extends
//! the store branch from where the last run ended (the watermark is derived
//! from the op-log, see [`watermark`]).
//!
//! ## What a commit becomes
//!
//! * Semantic ops: the commit's `lex.toml` + `lex.lock` + `src/**` are
//!   materialized into a private scratch dir (the loader needs a directory)
//!   and handed to the SAME [`publish_core::publish_dir`] `lex publish` uses,
//!   with `files: false`. Tree → ops and every gate are therefore one
//!   implementation; import and publish cannot disagree.
//! * A `SetFiles` op, LAST, under the same intent: the manifest of every other
//!   tracked file, built from git-object bytes (`src/**/*.lex`, `src.lex`,
//!   `.git` and top-level `.lex` are excluded — the manifest rejects reserved
//!   paths but does not filter).
//! * A tree with no `lex.toml` and no `src/**/*.lex` is a non-Lex repo: a
//!   manifest-only branch (zero semantic ops) — a legitimate shape (#892 §1.7).
//! * A commit that touches none of `src/**/*.lex`, `lex.toml`, `lex.lock`
//!   skips the semantic pass entirely: a `SetFiles`-only op. A commit with no
//!   net change is zero ops and leaves no trace in the op-log.
//! * A commit that deletes the whole package runs the semantic pass on an
//!   empty tree (`allow_empty`), so the removals are emitted.
//!
//! ## History shape
//!
//! First-parent only: a merge commit's tree diff (against its first parent)
//! folds the side branch into ONE intent, and `origin.parents` lists every
//! parent (the report adds the merged parents' subject lines). Renames are a
//! remove + add, as in `lex publish`.
//!
//! ## Commits that do not import (`--on-error fold|stop`)
//!
//! A commit the gates refuse (type-check, examples, the store's write-time
//! gate, an unusable tree) is skipped and — with `fold` (the default) — its
//! changes ride into the NEXT importable commit for free: the next publish
//! diffs against the last good head and the manifest is a full snapshot. That
//! commit's `origin.folded` lists the skipped SHAs (hashed), and the report
//! lists each with its phase and diagnostics. `stop` halts at the first
//! refusal; earlier commits stay imported. Exit 0 iff the tip landed, 2 if it
//! did not (the store is valid but stale at the last good commit) or `--strict`
//! fired on an unsupported path. The gate is never skipped: a broken head is
//! never imported.
//!
//! Historical commits are type-checked against TODAY'S dependencies: a package
//! with floating git dependencies and no tracked `lex.lock` folds heavily.
//! That is real, and the report's fold ratio says so.
//!
//! ## Determinism (what makes two imports converge on the same OpIds)
//!
//! Everything hashed is a pure function of the git objects: the prompt is the
//! commit message verbatim (lossy-UTF-8), the session is
//! `git-import:<first-parent root sha>` (repo identity is the root commit, not
//! the path or URL), the model is a constant, and author/committer/dates/parents
//! ride in the intent's `Origin`. `created_at` (unhashed) is the committer date. Bytes come
//! from `git diff-tree` / `git cat-file --batch`, never a checkout, so
//! `core.autocrlf`, `.gitattributes` and smudge filters cannot leak in.
//!
//! A shallow source (`--depth N` from a URL) makes the shallow boundary the
//! lineage root: a DIFFERENT session, so different OpIds than a full import of
//! the same repo, and the report says `shallow: true`. A shallow LOCAL repo is
//! refused for the same reason.
//!
//! ## Atomicity
//!
//! A commit's ops (semantic, then `SetFiles`) land on a private WORK branch
//! and the target branch is only moved (fast-forward) once the run is over.
//! Inside [`import_commit`] the work branch is checkpointed and restored on any
//! failure, so the loop never sees a half-applied commit. A run that fails
//! part-way leaves the target branch at the last good commit (or, for an
//! infrastructure error, advanced to it before the error is reported).
//!
//! ## Scale
//!
//! The manifest is an in-memory map updated only from each commit's changed
//! paths (`git diff-tree`), with a bounded oid → blob cache: per-commit cost is
//! O(changed files), not O(head). See [`tree`].

mod commit;
mod git;
mod report;
mod source;
mod tree;
mod watermark;

use crate::publish_core;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use commit::import_commit;
use git::*;
use lex_store::Store;
use lex_vcs::OpId;
use report::{render, Folded, Report, Stats};
use serde_json::{json, Value};
use source::Source;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tree::{ObjMeta, TreeState};

/// The importer's profile version, recorded as `model.version` of every
/// imported intent. It is HASHED (into the intent id, hence every OpId): bumping
/// it rotates every imported OpId, exactly like an `OperationFormat` bump. Change
/// it only when the mapping from a git commit to an intent/ops changes
/// incompatibly.
pub(crate) const IMPORT_PROFILE_VERSION: &str = "1";

/// A manifest file may not exceed this many bytes. Mirrors the hub's blob
/// limit; the hub only enforces it on push, so the importer refuses locally.
pub(crate) const MANIFEST_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// A manifest may not name more than this many files (the hub's limit too).
pub(crate) const MANIFEST_MAX_ENTRIES: usize = 10_000;

const USAGE: &str = "usage: lex op import-git <path|url> [--branch B] [--store DIR] \
[--store-branch S] [--head-only] [--on-error fold|stop] [--strict] \
[--examples tip|all|none] [--max-file-bytes N] [--depth N] [--since SHA] [--max-commits N]";

// ── arguments ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnError {
    Fold,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExamplesPolicy {
    Tip,
    All,
    None,
}

#[derive(Debug)]
struct ImportArgs {
    source: String,
    branch: Option<String>,
    store: Option<PathBuf>,
    store_branch: Option<String>,
    head_only: bool,
    on_error: OnError,
    strict: bool,
    examples: ExamplesPolicy,
    max_file_bytes: u64,
    depth: Option<u32>,
    since: Option<String>,
    max_commits: Option<usize>,
}

fn parse_args(args: &[String]) -> Result<ImportArgs> {
    let mut source: Option<String> = None;
    let mut a = ImportArgs {
        source: String::new(),
        branch: None,
        store: None,
        store_branch: None,
        head_only: false,
        on_error: OnError::Fold,
        strict: false,
        examples: ExamplesPolicy::Tip,
        max_file_bytes: MANIFEST_MAX_FILE_BYTES,
        depth: None,
        since: None,
        max_commits: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--branch" => a.branch = Some(value("--branch")?),
            "--store" => a.store = Some(PathBuf::from(value("--store")?)),
            "--store-branch" => a.store_branch = Some(value("--store-branch")?),
            "--head-only" => a.head_only = true,
            "--strict" => a.strict = true,
            "--on-error" => {
                a.on_error = match value("--on-error")?.as_str() {
                    "fold" => OnError::Fold,
                    "stop" => OnError::Stop,
                    o => bail!("--on-error must be `fold` or `stop`, not `{o}`"),
                }
            }
            "--examples" => {
                a.examples = match value("--examples")?.as_str() {
                    "tip" => ExamplesPolicy::Tip,
                    "all" => ExamplesPolicy::All,
                    "none" => ExamplesPolicy::None,
                    o => bail!("--examples must be `tip`, `all` or `none`, not `{o}`"),
                }
            }
            "--max-file-bytes" => {
                let v = value("--max-file-bytes")?;
                let n: u64 = v
                    .parse()
                    .map_err(|_| anyhow!("--max-file-bytes needs a byte count, not `{v}`"))?;
                if n > MANIFEST_MAX_FILE_BYTES {
                    bail!(
                        "--max-file-bytes can only lower the manifest limit \
                         ({MANIFEST_MAX_FILE_BYTES} bytes), not raise it"
                    );
                }
                a.max_file_bytes = n;
            }
            "--depth" => {
                let v = value("--depth")?;
                let n: u32 = v.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    anyhow!("--depth needs a positive number of commits, not `{v}`")
                })?;
                a.depth = Some(n);
            }
            "--since" => a.since = Some(value("--since")?),
            "--max-commits" => {
                let v = value("--max-commits")?;
                let n: usize = v.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    anyhow!("--max-commits needs a positive number of commits, not `{v}`")
                })?;
                a.max_commits = Some(n);
            }
            f if f.starts_with("--") => bail!("unexpected flag `{f}`\n{USAGE}"),
            p if source.is_none() => source = Some(p.to_string()),
            p => bail!("unexpected argument `{p}`\n{USAGE}"),
        }
    }
    a.source = source.ok_or_else(|| anyhow!("{USAGE}"))?;
    Ok(a)
}

// ── the import state and per-commit result ──────────────────────────────────

/// Everything [`import_commit`] needs, shared across the commits of one import.
struct ImportState {
    repo: PathBuf,
    cat: CatFile,
    info: ObjInfo,
    store_root: PathBuf,
    store: Store,
    /// The private branch commits are applied to; the target branch is only
    /// advanced onto it once the run is over.
    work_branch: String,
    /// First-parent root commit: the repo's identity (`git-import:<root>`).
    root_sha: String,
    /// Run the behavioural examples gate for the commit being imported (the
    /// loop sets it per commit from `--examples`).
    examples: bool,
    /// `--strict`: a commit whose tree holds an unsupported path is refused.
    strict: bool,
    max_file_bytes: u64,
    /// Skipped symlinks/submodules: each listed once, with first/last sighting.
    unsupported: Vec<Unsupported>,
    /// Commits skipped and folded into the next importable one; they land in
    /// that commit's `origin.folded`.
    pending_folded: Vec<String>,
    /// The incremental tree: the git tree of the last commit processed.
    tree: TreeState,
    /// The last commit the tree was advanced to (`None`: the next commit is a
    /// snapshot).
    prev: Option<String>,
    /// The store's semantic head may be behind the tree (a commit was refused
    /// or failed): the next commit re-runs the semantic pass even if it
    /// touches no Lex path, so a `SetFiles`-only commit can never land on top
    /// of a stale semantic head.
    dirty: bool,
    /// The private tree the loader reads: `lex.toml`, `lex.lock`, `src/**`,
    /// kept in step with `tree` by changed paths only.
    scratch: tempfile::TempDir,
    /// oid → what is known about the object (size, stored blob): a bound on
    /// object reads, not a correctness mechanism.
    cache: HashMap<String, ObjMeta>,
    stats: Stats,
    /// The parents of the commit `import_commit` last read (for the report's
    /// `merged` subject lines).
    last_parents: Vec<String>,
    /// Test seam: fail after the semantic ops landed, before the `SetFiles`.
    #[cfg(test)]
    fail_after_semantic: bool,
    /// Test seam: whether the semantic ops were visible on the work branch when
    /// the failure was injected (so the rollback test is not vacuous).
    #[cfg(test)]
    saw_partial_head: bool,
}

impl ImportState {
    /// Open the import: a fresh private work branch forked from `store_branch`
    /// (absent, empty, or the branch being extended), the `cat-file`
    /// processes, the scratch tree and the limits.
    fn open(
        repo: PathBuf,
        store_root: PathBuf,
        store: Store,
        store_branch: &str,
        root_sha: String,
        a: &ImportArgs,
    ) -> Result<ImportState> {
        let work_branch = format!("import-work-{}", std::process::id());
        if store.get_branch(&work_branch)?.is_some() {
            delete_branch(&store_root, &store, &work_branch)?;
        }
        store.create_branch(&work_branch, store_branch).map_err(|e| anyhow!("{e}"))?;
        Ok(ImportState {
            cat: CatFile::spawn(&repo)?,
            info: ObjInfo::spawn(&repo)?,
            repo,
            store_root,
            store,
            work_branch,
            root_sha,
            examples: a.examples != ExamplesPolicy::None,
            strict: a.strict,
            max_file_bytes: a.max_file_bytes,
            unsupported: Vec::new(),
            pending_folded: Vec::new(),
            tree: TreeState::default(),
            prev: None,
            dirty: false,
            scratch: tempfile::Builder::new()
                .prefix("lex-import-git-")
                .tempdir()
                .context("creating scratch dir")?,
            cache: HashMap::new(),
            stats: Stats::default(),
            last_parents: Vec::new(),
            #[cfg(test)]
            fail_after_semantic: false,
            #[cfg(test)]
            saw_partial_head: false,
        })
    }
}

/// A symlink or submodule the importer skipped. A persistent one is listed
/// once: where it was first seen and where it was last seen.
#[derive(Debug, Clone)]
struct Unsupported {
    path: String,
    kind: &'static str,
    first_seen: String,
    last_seen: String,
}

/// Why a commit was not imported. `phase` says where; `reason` is a stable tag.
#[derive(Debug, Clone)]
struct Refusal {
    reason: &'static str,
    phase: &'static str,
    message: String,
    diagnostics: Vec<Value>,
}

impl Refusal {
    fn new(reason: &'static str, phase: &'static str, message: impl Into<String>) -> Self {
        Refusal { reason, phase, message: message.into(), diagnostics: Vec::new() }
    }
}

#[derive(Debug)]
enum CommitOutcome {
    /// The commit produced `ops` ops (semantic ones plus the `SetFiles`), the
    /// last of which is `files_op` when a manifest changed. `head` is the work
    /// branch's head afterwards.
    #[allow(dead_code)] // read by the atomicity unit test
    Imported { ops: usize, files_op: Option<OpId>, head: Option<OpId> },
    /// The commit changed nothing the store tracks.
    Noop,
    /// Nothing landed for this commit; the work branch is unchanged.
    Refused(Refusal),
}

/// Whether `lex.toml`'s bytes declare `[package] name`.
fn has_package_name(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(s).ok())
        .and_then(|v| v.get("package")?.get("name")?.as_str().map(|n| !n.trim().is_empty()))
        .unwrap_or(false)
}

/// Delete a branch and the by-products the store keeps beside its file.
fn delete_branch(root: &Path, store: &Store, name: &str) -> Result<()> {
    store.delete_branch(name).map_err(|e| anyhow!("{e}"))?;
    for suffix in ["head_snapshot.json", "lock"] {
        let _ = std::fs::remove_file(root.join("branches").join(format!("{name}.{suffix}")));
    }
    Ok(())
}

// ── the command ─────────────────────────────────────────────────────────────

pub fn cmd_import_git(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let a = parse_args(args)?;
    let report = run_import(&a)?;
    render(fmt, &report);
    if report.landed() {
        Ok(())
    } else {
        std::process::exit(2);
    }
}

fn run_import(a: &ImportArgs) -> Result<Report> {
    let started = std::time::Instant::now();
    if a.head_only && (a.since.is_some() || a.max_commits.is_some()) {
        bail!("--head-only imports one snapshot; it cannot be combined with --since or --max-commits");
    }
    let src = Source::open(&a.source, a.depth, a.branch.as_deref())?;
    let repo = src.repo.clone();

    // The tip: `--branch` (a branch of the repo) or the branch HEAD points at.
    let git_branch = match &a.branch {
        Some(b) => b.clone(),
        None => git_line(&repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| {
            anyhow!("HEAD is detached or unborn in {}; pass --branch <name>", src.display)
        })?,
    };
    let tip = git_line(
        &repo,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{git_branch}^{{commit}}")],
    )
    .map_err(|_| source::no_branch(&git_branch, &src))?;
    let root_sha = git_line(&repo, &["rev-list", "--first-parent", "--max-parents=0", &tip])?
        .lines()
        .last()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no root commit found from {tip}"))?;

    let store_branch = a.store_branch.clone().unwrap_or_else(|| git_branch.clone());
    if store_branch.is_empty() || store_branch.contains('/') || store_branch.contains('\\') {
        bail!(
            "`{store_branch}` cannot be a store branch name (empty or path-like); \
             pass --store-branch <name>"
        );
    }
    let store_root = a.store.clone().unwrap_or_else(crate::default_store_root_pub);
    let store = Store::open(&store_root).with_context(|| format!("opening store at {}", store_root.display()))?;

    // ── where does this run start? ──────────────────────────────────────────
    let wm = match watermark::derive(&store, &store_branch)? {
        Ok(w) => w,
        Err(watermark::WatermarkRefusal(msg)) => bail!("{msg}"),
    };
    if let Some(w) = &wm {
        if a.head_only {
            bail!(
                "store branch `{store_branch}` already has history (last imported commit {}); \
                 --head-only imports a snapshot into an EMPTY branch. Drop --head-only to import \
                 the commits since, or import into a fresh branch with --store-branch <name>",
                w.commit
            );
        }
        if a.since.is_some() {
            bail!(
                "--since starts a NEW lineage at a commit and only applies to an empty store branch; \
                 `{store_branch}` already has history (last imported commit {})",
                w.commit
            );
        }
        let expect = format!("git-import:{root_sha}");
        if w.session != expect {
            bail!(
                "store branch `{store_branch}` was imported from a different repository (or a \
                 shallow/full variant of this one): its lineage is `{}`, this source's is `{expect}`. \
                 OpIds would not converge; import into a fresh branch with --store-branch <name>",
                w.session
            );
        }
    }

    let chain = if a.head_only { vec![tip.clone()] } else { first_parent_chain(&repo, &tip)? };
    let mut start = 0usize;
    let mut since_sha = None;
    if let Some(w) = &wm {
        match chain.iter().position(|c| *c == w.commit) {
            Some(i) => start = i + 1,
            None => {
                let exists = git_ok(&repo, &["cat-file", "-e", &format!("{}^{{commit}}", w.commit)])?;
                let why = if !exists {
                    "not in this repository at all (a different repo, or history that was rewritten)"
                } else if !git_ok(&repo, &["merge-base", "--is-ancestor", &w.commit, &tip])? {
                    "not an ancestor of the branch tip (history was rewritten, amended or force-pushed)"
                } else {
                    "reachable from the tip only through a merge's second parent, not on its first-parent history"
                };
                bail!(
                    "store branch `{store_branch}` was last imported at {}, which is {why}. Importing \
                     must never rewrite history already in the store: import into a NEW branch with \
                     --store-branch <name> (the existing branch is left untouched)",
                    w.commit
                );
            }
        }
    } else if let Some(s) = &a.since {
        let sha = git_line(&repo, &["rev-parse", "--verify", "--quiet", &format!("{s}^{{commit}}")])
            .map_err(|_| anyhow!("--since {s}: no such commit in {}", src.display))?;
        start = chain.iter().position(|c| *c == sha).ok_or_else(|| {
            anyhow!("--since {s} is not on the first-parent history of `{git_branch}` ({tip})")
        })?;
        since_sha = Some(sha);
    }
    let mut commits: Vec<String> = chain[start..].to_vec();
    let mut remaining = 0usize;
    if let Some(n) = a.max_commits {
        if commits.len() > n {
            remaining = commits.len() - n;
            commits.truncate(n);
        }
    }
    let effective_tip = commits.last().cloned().unwrap_or_else(|| tip.clone());

    let mut report = Report {
        source: src.display.clone(),
        source_kind: if src.is_url { "url" } else { "path" },
        shallow: src.shallow,
        head_only: a.head_only,
        git_branch: git_branch.clone(),
        store_branch: store_branch.clone(),
        watermark: wm.as_ref().map(|w| w.commit.clone()),
        since: since_sha.clone(),
        snapshot_base: None,
        imported: Vec::new(),
        folded: Vec::new(),
        noop: Vec::new(),
        unsupported: Vec::new(),
        tip_sha: effective_tip,
        requested_tip: tip.clone(),
        remaining,
        tip_landed: false,
        failed: None,
        strict_violation: false,
        head_op: None,
        total: 0,
        blob_reads: 0,
        size_queries: 0,
        stats: Stats::default(),
        elapsed_ms: 0,
        notes: Vec::new(),
    };
    // Where did this lineage start? A snapshot start (`--head-only`/`--since`)
    // leaves earlier history out for good; a later run inherits that fact from
    // the previous run's local report (a convenience copy: if it is gone, so is
    // the note — the op-log itself has no such marker).
    report.snapshot_base = match (&wm, a.head_only, &since_sha) {
        (None, true, _) => Some(json!({ "commit": tip, "via": "head-only" })),
        (None, false, Some(s)) => Some(json!({ "commit": s, "via": "since" })),
        (Some(_), ..) => std::fs::read(store_root.join("import").join(format!("{store_branch}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("snapshot_base").cloned())
            .filter(|v| !v.is_null()),
        _ => None,
    };
    if wm.is_some() {
        if let Some(sb) = &report.snapshot_base {
            report.notes.push(format!(
                "history before {} (a --{} snapshot) was never imported and is not backfilled",
                sb["commit"].as_str().unwrap_or("?"),
                sb["via"].as_str().unwrap_or("snapshot")
            ));
        }
    }
    if a.head_only {
        report.notes.push(
            "--head-only imports the tip as one snapshot; history before it is NOT imported. A later \
             full import continues forward from this snapshot and does not backfill earlier history"
                .into(),
        );
    }
    if let Some(s) = &since_sha {
        report.notes.push(format!(
            "--since: the lineage starts at {s} (imported as a snapshot); history before it is NOT imported \
             and is not backfilled by later runs"
        ));
    }
    if src.shallow {
        report.notes.push(
            "shallow import: the shallow boundary is the lineage root, so the session (and every OpId) \
             differs from a full-history import of the same repository"
                .into(),
        );
    }
    if remaining > 0 {
        report.notes.push(format!(
            "--max-commits stopped the run with {remaining} commit(s) left; run again to continue"
        ));
    }

    if commits.is_empty() {
        // Nothing new: the watermark is the tip.
        report.tip_landed = true;
        report.head_op = store.get_branch(&store_branch)?.and_then(|b| b.head_op);
        report.elapsed_ms = started.elapsed().as_millis();
        report.write_file(&store_root);
        return Ok(report);
    }

    let mut state = ImportState::open(repo.clone(), store_root.clone(), store, &store_branch, root_sha, a)?;
    let work_branch = state.work_branch.clone();
    let outcome = walk(&mut state, a, &wm, &commits, &mut report);
    report.unsupported = std::mem::take(&mut state.unsupported);
    report.stats = state.stats.clone();
    report.blob_reads = state.cat.reads;
    report.size_queries = state.info.queries;

    // The single step that publishes the import: move the target branch onto
    // the work branch's head — also after a failure, so earlier (atomic,
    // gated) commits stay imported.
    let work_head = state.store.get_branch(&work_branch)?.and_then(|b| b.head_op);
    let advance = match (&work_head, report.imported.is_empty()) {
        (Some(h), false) => state.store.advance_branch_head_ff(&store_branch, h).map(|_| ()),
        _ => Ok(()),
    };
    let cleanup = delete_branch(&state.store_root, &state.store, &work_branch);
    outcome?;
    advance.map_err(|e| anyhow!("{e}"))?;
    cleanup?;
    report.head_op = state.store.get_branch(&store_branch)?.and_then(|b| b.head_op);
    report.elapsed_ms = started.elapsed().as_millis();
    report.write_file(&store_root);
    Ok(report)
}

/// The history loop: `import_commit` once per commit, folding or stopping on a
/// refusal per `--on-error`.
fn walk(
    state: &mut ImportState,
    a: &ImportArgs,
    wm: &Option<watermark::Watermark>,
    commits: &[String],
    report: &mut Report,
) -> Result<()> {
    if let Some(w) = wm {
        // Bring the incremental tree to the last imported commit: the store
        // already holds exactly that tree's semantic state.
        state
            .advance_tree(&w.commit)
            .with_context(|| format!("reading the last imported commit {}", w.commit))?;
    }
    let mut pending_idx: Vec<usize> = Vec::new();
    let n = commits.len();
    for (i, sha) in commits.iter().enumerate() {
        let is_last = i + 1 == n;
        state.examples = match a.examples {
            ExamplesPolicy::All => true,
            ExamplesPolicy::None => false,
            ExamplesPolicy::Tip => is_last,
        };
        report.total += 1;
        match import_commit(state, sha)? {
            CommitOutcome::Imported { ops, files_op, .. } => {
                let mut entry = json!({ "sha": sha, "ops": ops, "files_op": files_op });
                if state.last_parents.len() > 1 {
                    entry["merged"] = state.last_parents[1..]
                        .iter()
                        .map(|p| json!({ "sha": p, "subject": subject_of(&state.repo, p) }))
                        .collect();
                }
                for idx in pending_idx.drain(..) {
                    report.folded[idx].into = Some(sha.clone());
                }
                report.imported.push(entry);
                report.tip_landed = is_last;
            }
            CommitOutcome::Noop => {
                report.noop.push(sha.clone());
                report.tip_landed = is_last;
            }
            CommitOutcome::Refused(r) => {
                let halt = a.on_error == OnError::Stop || a.strict || r.reason == "strict:unsupported";
                if halt || is_last {
                    if r.reason == "strict:unsupported" {
                        report.strict_violation = true;
                    }
                    report.failed = Some((sha.clone(), r));
                    report.tip_landed = false;
                    break;
                }
                state.pending_folded.push(sha.clone());
                pending_idx.push(report.folded.len());
                report.folded.push(Folded { sha: sha.clone(), refusal: r, into: None });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_cmd(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.org")
            .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00+0000")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.org")
            .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00+0000")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn state_for(repo: &Path, store_root: &Path, root: String) -> ImportState {
        let args = parse_args(&[repo.display().to_string(), "--head-only".into()]).unwrap();
        let store = Store::open(store_root).unwrap();
        ImportState::open(repo.to_path_buf(), store_root.to_path_buf(), store, "main", root, &args).unwrap()
    }

    fn work_head(s: &ImportState) -> Option<OpId> {
        s.store.get_branch(&s.work_branch).unwrap().and_then(|b| b.head_op)
    }

    /// A commit is atomic: if anything fails AFTER its semantic ops landed (here
    /// injected before the `SetFiles`), the work branch is put back exactly as it
    /// was — no half-applied commit for PR 5's loop to build on — and the same
    /// commit then imports cleanly.
    ///
    /// Mutation: removing the restore in `import_commit` leaves the semantic ops
    /// on the work branch and this test red.
    #[test]
    fn a_failure_after_the_semantic_ops_rolls_the_commit_back() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("lex.toml"), "[package]\nname = \"atomic\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(repo.join("src/main.lex"), "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n").unwrap();
        std::fs::write(repo.join("README.md"), "hi\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "atomic"]);
        let tip = git(&repo, &["rev-parse", "HEAD"]);

        let store_root = t.path().join("store");
        let mut state = state_for(&repo, &store_root, tip.clone());
        assert_eq!(work_head(&state), None);

        state.fail_after_semantic = true;
        let err = import_commit(&mut state, &tip).expect_err("the injected failure propagates");
        assert!(format!("{err:#}").contains("injected"));
        assert!(state.saw_partial_head, "the semantic ops WERE on the branch when it failed");
        assert_eq!(work_head(&state), None, "...and the rollback removed them");
        assert_eq!(
            state.store.list_branches().unwrap().into_iter().filter(|b| state.store.get_branch(b).unwrap().is_some()).collect::<Vec<_>>(),
            vec![state.work_branch.clone()],
            "no checkpoint branch is left behind"
        );
        assert!(state.store.branch_head(&state.work_branch).unwrap().is_empty());

        state.fail_after_semantic = false;
        match import_commit(&mut state, &tip).unwrap() {
            CommitOutcome::Imported { ops, files_op, head } => {
                assert_eq!(ops, 3, "two AddFunction + one SetFiles");
                assert!(files_op.is_some());
                assert_eq!(work_head(&state), head);
            }
            other => panic!("expected Imported, got {other:?}"),
        }
        assert_eq!(state.store.branch_head(&state.work_branch).unwrap().len(), 2);
        // Importing the same commit again changes nothing.
        assert!(matches!(import_commit(&mut state, &tip).unwrap(), CommitOutcome::Noop));
    }

    #[test]
    fn commit_objects_are_parsed_verbatim() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a"), "a").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "--cleanup=verbatim", "-m", "subject\n\n\nbody  \n\n"]);
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        let m = read_commit(&repo, &sha).unwrap();
        assert_eq!(m.message, "subject\n\n\nbody  \n\n", "verbatim: blank lines and trailing spaces survive");
        assert!(m.parents.is_empty());
        assert_eq!((m.author.name.as_str(), m.author.tz.as_str(), m.author.when), ("t", "+0000", 1_704_067_200));

        let p = parse_person("A <b> C <c@d.e> 1700000000 -0930").unwrap();
        assert_eq!((p.name.as_str(), p.email.as_str(), p.when, p.tz.as_str()), ("A <b> C", "c@d.e", 1_700_000_000, "-0930"));
        let empty = parse_person(" <> 5 +0100").unwrap();
        assert_eq!((empty.name.as_str(), empty.email.as_str()), ("", ""));
    }
}
