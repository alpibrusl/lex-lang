//! `lex op import-git` — the first working git → op-log importer (#892 PR 4).
//!
//! ```text
//! lex op import-git <path|url> [--branch B] [--store DIR] [--store-branch S]
//!                   [--head-only] [--on-error fold|stop] [--strict]
//!                   [--examples tip|all|none] [--max-file-bytes N]
//! ```
//!
//! **This PR imports the TIP of one branch as ONE snapshot** (`--head-only`,
//! required). Full history, URL clones, incremental re-import and the
//! `--on-error` policies are PR 5; the flags are parsed now so the surface does
//! not change, and the per-commit work is [`import_commit`], which PR 5's loop
//! calls once per commit.
//!
//! ## What a commit becomes
//!
//! * Semantic ops: the commit's `lex.toml` + `lex.lock` + `src/**` are
//!   materialized into a private scratch dir (the loader needs a directory)
//!   and handed to the SAME [`publish_core::publish_dir`] `lex publish` uses,
//!   with `files: false`. Tree → ops and every gate are therefore one
//!   implementation; import and publish cannot disagree.
//! * A `SetFiles` op, LAST, under the same intent: the manifest of every other
//!   tracked file, built from git-object bytes with `manifest_from_files`
//!   (`src/**/*.lex`, `src.lex`, `.git` and top-level `.lex` are excluded here —
//!   `manifest_from_files` rejects reserved paths but does not filter).
//! * A tree with no `lex.toml` and no `src/**/*.lex` is a non-Lex repo: a
//!   manifest-only branch (zero semantic ops) — a legitimate shape (#892 §1.7).
//!
//! ## Determinism (what makes two imports converge on the same OpIds)
//!
//! Everything hashed is a pure function of the git objects: the prompt is the
//! commit message verbatim (lossy-UTF-8), the session is
//! `git-import:<first-parent root sha>` (repo identity is the root commit, not
//! the path or URL), the model is a constant, and author/committer/dates/parents
//! ride in [`Origin`]. `created_at` (unhashed) is the committer date. Bytes come
//! from `git ls-tree` / `git cat-file --batch`, never a checkout, so
//! `core.autocrlf`, `.gitattributes` and smudge filters cannot leak in.
//!
//! ## Atomicity
//!
//! A commit's ops (semantic, then `SetFiles`) land on a private WORK branch and
//! the target branch is only moved (fast-forward from empty) once every step
//! succeeded. Inside [`import_commit`] the work branch is checkpointed and
//! restored on any failure, so PR 5's per-commit loop never sees a half-applied
//! commit. A failed import therefore leaves the target branch exactly as it
//! was (the only residue is unreachable content-addressed objects).

use crate::publish_core::{self, Outcome, PublishError, PublishOptions};
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_store::files::{is_reserved_path, validate_path, MODE_EXEC, MODE_FILE};
use lex_store::Store;
use lex_vcs::{Intent, IntentLog, ModelDescriptor, OpId, Origin, Person};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
[--examples tip|all|none] [--max-file-bytes N]";

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
    // Parsed now so the flag surface is final; only PR 5's history loop acts on it.
    #[allow(dead_code)]
    on_error: OnError,
    strict: bool,
    examples: ExamplesPolicy,
    max_file_bytes: u64,
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
            f if f.starts_with("--") => bail!("unexpected flag `{f}`\n{USAGE}"),
            p if source.is_none() => source = Some(p.to_string()),
            p => bail!("unexpected argument `{p}`\n{USAGE}"),
        }
    }
    a.source = source.ok_or_else(|| anyhow!("{USAGE}"))?;
    Ok(a)
}

/// Whether `s` names a remote (a URL or scp-style address) rather than a path.
fn looks_like_url(s: &str) -> bool {
    if s.contains("://") {
        return true;
    }
    // scp-like `user@host:path` / `host:path` — but not an existing local path.
    !Path::new(s).exists() && s.contains(':') && !s.starts_with('/') && !s.starts_with('.')
}

// ── git plumbing (objects only) ─────────────────────────────────────────────

/// A `git` invocation pinned to `dir`, with the caller's repo-selecting
/// environment scrubbed (so a stray `GIT_DIR` cannot redirect the read) and
/// nothing ever prompting.
fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    for v in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
    ] {
        c.env_remove(v);
    }
    c.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C").env("GIT_OPTIONAL_LOCKS", "0");
    c
}

fn git_out(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git_cmd(dir)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}` (is git installed?)", args.join(" ")))?;
    if !out.status.success() {
        bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn git_line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_out(dir, args)?).trim().to_string())
}

/// One long-lived `git cat-file --batch`: blob bytes straight from the object
/// database, with no smudge/eol/attributes processing.
struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    fn spawn(dir: &Path) -> Result<CatFile> {
        let mut child = git_cmd(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning `git cat-file --batch`")?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok(CatFile { child, stdin, stdout })
    }

    fn read(&mut self, oid: &str) -> Result<Vec<u8>> {
        self.stdin
            .write_all(format!("{oid}\n").as_bytes())
            .and_then(|_| self.stdin.flush())
            .context("writing to `git cat-file --batch`")?;
        let mut header = String::new();
        self.stdout.read_line(&mut header).context("reading `git cat-file --batch`")?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        let size: usize = match parts.as_slice() {
            [_, "blob", size] => size.parse().map_err(|_| anyhow!("bad cat-file header `{}`", header.trim()))?,
            [_, "missing"] => bail!("git object {oid} is missing from the repository"),
            _ => bail!("unexpected cat-file header `{}` for {oid}", header.trim()),
        };
        let mut buf = vec![0u8; size];
        self.stdout.read_exact(&mut buf).with_context(|| format!("reading git object {oid}"))?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl).context("reading cat-file terminator")?;
        Ok(buf)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One tree entry, straight from `git ls-tree -r -z -l`.
#[derive(Debug, Clone)]
struct TreeEntry {
    mode: u32,
    kind: String,
    oid: String,
    size: u64,
    /// Raw path bytes (git paths are bytes; non-UTF-8 ones are refused).
    path: Vec<u8>,
}

fn ls_tree(dir: &Path, sha: &str) -> Result<Vec<TreeEntry>> {
    let raw = git_out(dir, &["ls-tree", "-r", "-z", "-l", "--full-tree", sha])?;
    let mut out = Vec::new();
    for rec in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = rec
            .iter()
            .position(|&b| b == b'\t')
            .ok_or_else(|| anyhow!("malformed ls-tree record"))?;
        let meta = String::from_utf8_lossy(&rec[..tab]).to_string();
        let f: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind, oid, size] = f.as_slice() else {
            bail!("malformed ls-tree record `{meta}`");
        };
        out.push(TreeEntry {
            mode: u32::from_str_radix(mode, 8).map_err(|_| anyhow!("bad mode `{mode}`"))?,
            kind: kind.to_string(),
            oid: oid.to_string(),
            // A submodule (`commit`) has no size (`-`).
            size: size.parse().unwrap_or(0),
            path: rec[tab + 1..].to_vec(),
        });
    }
    Ok(out)
}

/// A parsed git commit object.
#[derive(Debug, Clone)]
struct CommitMeta {
    sha: String,
    parents: Vec<String>,
    author: Person,
    committer: Person,
    /// The message verbatim (lossy UTF-8, so the decode is deterministic).
    message: String,
}

/// Parse `git cat-file commit <sha>` directly — never localized `git log`
/// output. Headers end at the first blank line; a continuation line (a
/// `gpgsig` / `mergetag` body) starts with a space and is skipped.
fn read_commit(dir: &Path, sha: &str) -> Result<CommitMeta> {
    let raw = git_out(dir, &["cat-file", "commit", sha])?;
    let split = raw.windows(2).position(|w| w == b"\n\n");
    let (head, body) = match split {
        Some(i) => (&raw[..i], &raw[i + 2..]),
        None => (&raw[..], &raw[raw.len()..]),
    };
    let head = String::from_utf8_lossy(head);
    let (mut parents, mut author, mut committer) = (Vec::new(), None, None);
    for line in head.split('\n') {
        if line.starts_with(' ') {
            continue;
        }
        let (key, val) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "parent" => parents.push(val.to_string()),
            "author" if author.is_none() => author = Some(parse_person(val)?),
            "committer" if committer.is_none() => committer = Some(parse_person(val)?),
            _ => {}
        }
    }
    Ok(CommitMeta {
        sha: sha.to_string(),
        parents,
        author: author.ok_or_else(|| anyhow!("commit {sha} has no author"))?,
        committer: committer.ok_or_else(|| anyhow!("commit {sha} has no committer"))?,
        message: String::from_utf8_lossy(body).into_owned(),
    })
}

/// `Name <email> 1700000000 +0200` → [`Person`]. The zone is kept verbatim.
fn parse_person(s: &str) -> Result<Person> {
    let gt = s.rfind('>').ok_or_else(|| anyhow!("malformed identity `{s}`"))?;
    let lt = s[..gt].rfind('<').ok_or_else(|| anyhow!("malformed identity `{s}`"))?;
    let name = s[..lt].trim_end().to_string();
    let email = s[lt + 1..gt].to_string();
    let mut rest = s[gt + 1..].split_whitespace();
    let when: i64 = rest
        .next()
        .and_then(|w| w.parse().ok())
        .ok_or_else(|| anyhow!("malformed identity date in `{s}`"))?;
    let tz = rest.next().unwrap_or("+0000").to_string();
    Ok(Person { name, email, when, tz })
}

// ── the import state and per-commit result ──────────────────────────────────

/// Everything [`import_commit`] needs, shared across the commits of one import.
struct ImportState {
    repo: PathBuf,
    cat: CatFile,
    store_root: PathBuf,
    store: Store,
    /// The private branch commits are applied to; the target branch is only
    /// advanced onto it after the whole import succeeded.
    work_branch: String,
    /// First-parent root commit: the repo's identity (`git-import:<root>`).
    root_sha: String,
    /// Run the behavioural examples gate for the commit being imported.
    examples: bool,
    max_file_bytes: u64,
    /// Skipped symlinks/submodules, in encounter order.
    unsupported: Vec<Unsupported>,
    /// Commits skipped and folded into the next importable one (PR 5; always
    /// empty in the tip-only import).
    pending_folded: Vec<String>,
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
    /// (absent or empty in this PR), the `cat-file` process, and the limits.
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
            repo,
            store_root,
            store,
            work_branch,
            root_sha,
            examples: a.examples != ExamplesPolicy::None,
            max_file_bytes: a.max_file_bytes,
            unsupported: Vec::new(),
            pending_folded: Vec::new(),
            #[cfg(test)]
            fail_after_semantic: false,
            #[cfg(test)]
            saw_partial_head: false,
        })
    }
}

#[derive(Debug, Clone)]
struct Unsupported {
    path: String,
    kind: &'static str,
    commit: String,
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
    Imported { ops: usize, files_op: Option<OpId>, head: Option<OpId> },
    /// The commit changed nothing the store tracks.
    Noop,
    /// Nothing landed for this commit; the work branch is unchanged.
    Refused(Refusal),
}

/// The deterministic intent of `meta` (see the module docs).
fn build_intent(root_sha: &str, meta: &CommitMeta, folded: &[String]) -> Intent {
    let origin = Origin {
        vcs: "git".into(),
        commit: meta.sha.clone(),
        author: meta.author.clone(),
        committer: Some(meta.committer.clone()),
        parents: meta.parents.clone(),
        folded: folded.to_vec(),
    };
    Intent::with_timestamp(
        meta.message.clone(),
        format!("git-import:{root_sha}"),
        ModelDescriptor {
            provider: "git".into(),
            name: "import".into(),
            version: Some(IMPORT_PROFILE_VERSION.into()),
        },
        None,
        meta.committer.when.max(0) as u64,
    )
    .with_origin(origin)
}

// ── one commit ──────────────────────────────────────────────────────────────

/// A tracked file the commit will carry, classified.
struct KeptFile {
    path: String,
    oid: String,
    exec: bool,
}

/// The classified tree of one commit.
struct Classified {
    /// Everything but `src/**/*.lex`: goes into the manifest.
    manifest: Vec<KeptFile>,
    /// `src/**/*.lex` (op-log-owned): loaded by the semantic pass.
    lex_sources: Vec<KeptFile>,
    has_lex_toml: bool,
    has_lex_lock: bool,
    has_root_src_lex: bool,
}

/// Classify `sha`'s tree and run the local checks the hub only enforces on
/// push. Nothing is written and no blob is read.
fn classify_tree(state: &mut ImportState, sha: &str) -> Result<std::result::Result<Classified, Refusal>> {
    let mut c = Classified {
        manifest: Vec::new(),
        lex_sources: Vec::new(),
        has_lex_toml: false,
        has_lex_lock: false,
        has_root_src_lex: false,
    };
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    for e in ls_tree(&state.repo, sha)? {
        let Ok(path) = String::from_utf8(e.path.clone()) else {
            return Ok(Err(Refusal::new(
                "manifest:path",
                "tree",
                format!("a path is not valid UTF-8: {}", String::from_utf8_lossy(&e.path)),
            )));
        };
        // Symlinks and submodules are not representable (files-v1): skip + list.
        let kind = match (e.mode & 0o170000, e.kind.as_str()) {
            (0o120000, _) => Some("symlink"),
            (0o160000, _) | (_, "commit") => Some("submodule"),
            _ => None,
        };
        if let Some(kind) = kind {
            state.unsupported.push(Unsupported { path, kind, commit: sha.to_string() });
            continue;
        }
        // The store's own directory / git internals are never content.
        let first = path.split('/').next().unwrap_or("");
        if path.split('/').any(|c| c.eq_ignore_ascii_case(".git")) || first.eq_ignore_ascii_case(".lex") {
            continue;
        }
        if path.split('/').any(|c| c.is_empty() || c == "." || c == "..") || path.contains('\0') {
            return Ok(Err(Refusal::new("manifest:path", "tree", format!("unusable path `{path}`"))));
        }
        if e.size > state.max_file_bytes {
            return Ok(Err(Refusal::new(
                "manifest:limit",
                "manifest",
                format!(
                    "`{path}` is {} bytes, over the {}-byte per-file limit",
                    e.size, state.max_file_bytes
                ),
            )));
        }
        if let Some(prev) = folded.insert(path.to_lowercase(), path.clone()) {
            return Ok(Err(Refusal::new(
                "manifest:case_collision",
                "manifest",
                format!("paths `{prev}` and `{path}` differ only in case"),
            )));
        }
        let file = KeptFile { path: path.clone(), oid: e.oid, exec: e.mode & 0o111 != 0 };
        if path == "src.lex" {
            c.has_root_src_lex = true;
        } else if is_reserved_path(&path) {
            c.lex_sources.push(file);
        } else {
            match path.as_str() {
                "lex.toml" => c.has_lex_toml = true,
                "lex.lock" => c.has_lex_lock = true,
                _ => {}
            }
            if let Err(err) = validate_path(&path) {
                return Ok(Err(Refusal::new("manifest:path", "manifest", err.to_string())));
            }
            c.manifest.push(file);
        }
    }
    if c.manifest.len() > MANIFEST_MAX_ENTRIES {
        return Ok(Err(Refusal::new(
            "manifest:limit",
            "manifest",
            format!(
                "{} files, over the {MANIFEST_MAX_ENTRIES}-entry manifest limit",
                c.manifest.len()
            ),
        )));
    }
    Ok(Ok(c))
}

/// Whether `lex.toml`'s bytes declare `[package] name`.
fn has_package_name(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(s).ok())
        .and_then(|v| v.get("package")?.get("name")?.as_str().map(|n| !n.trim().is_empty()))
        .unwrap_or(false)
}

/// Import ONE commit onto `state.work_branch`, atomically: semantic ops first,
/// `SetFiles` last, one intent — or nothing at all. PR 5's history loop calls
/// this once per commit.
///
/// `Ok(Refused(..))` is an expected outcome (a gate said no); `Err` is an
/// infrastructure failure (git, IO). Either way the work branch is restored.
fn import_commit(state: &mut ImportState, sha: &str) -> Result<CommitOutcome> {
    let meta = read_commit(&state.repo, sha)?;
    let cls = match classify_tree(state, sha)? {
        Ok(c) => c,
        Err(refusal) => return Ok(CommitOutcome::Refused(refusal)),
    };
    let is_package = if cls.lex_sources.is_empty() && !cls.has_root_src_lex {
        false
    } else {
        // `src/**/*.lex` (or `src.lex`) present: those paths are reserved (they
        // cannot go in a manifest), so the tree MUST be a loadable package.
        if cls.has_root_src_lex {
            return Ok(CommitOutcome::Refused(Refusal::new(
                "lex:root_src_lex",
                "tree",
                "`src.lex` at the repository root is op-log-owned and not importable; \
                 a Lex package keeps its sources under src/",
            )));
        }
        if !cls.has_lex_toml {
            return Ok(CommitOutcome::Refused(Refusal::new(
                "lex:no_manifest",
                "tree",
                "the tree has src/**/*.lex but no lex.toml: those paths are reserved for the \
                 op-log and cannot be carried in a files manifest, so this is not importable",
            )));
        }
        let toml = state.cat.read(&cls.manifest.iter().find(|f| f.path == "lex.toml").expect("has_lex_toml").oid)?;
        if !has_package_name(&toml) {
            return Ok(CommitOutcome::Refused(Refusal::new(
                "lex:not_a_package",
                "tree",
                "the tree has src/**/*.lex but its lex.toml has no [package] name",
            )));
        }
        true
    };

    let intent = build_intent(&state.root_sha, &meta, &state.pending_folded);
    let work = state.work_branch.clone();
    let ckpt = format!("{work}.ckpt");
    state.store.create_branch(&ckpt, &work).map_err(|e| anyhow!("{e}"))?;

    let result = apply_commit(state, &cls, is_package, &intent);

    match &result {
        Ok(CommitOutcome::Imported { .. }) => {
            delete_branch(&state.store_root, &state.store, &ckpt)?;
            state.pending_folded.clear();
        }
        // Refused, Noop or an infrastructure error: put the work branch back
        // exactly as it was before this commit.
        _ => {
            delete_branch(&state.store_root, &state.store, &work)?;
            state.store.create_branch(&work, &ckpt).map_err(|e| anyhow!("{e}"))?;
            delete_branch(&state.store_root, &state.store, &ckpt)?;
        }
    }
    result
}

/// The mutating half of [`import_commit`]; the caller owns the checkpoint.
fn apply_commit(
    state: &mut ImportState,
    cls: &Classified,
    is_package: bool,
    intent: &Intent,
) -> Result<CommitOutcome> {
    let work = state.work_branch.clone();
    let mut ops = 0usize;
    let mut lock_toml: Option<String> = None;

    // ── the semantic pass ───────────────────────────────────────────────────
    if is_package {
        let scratch = tempfile::Builder::new().prefix("lex-import-git-").tempdir().context("creating scratch dir")?;
        materialize(state, cls, scratch.path(), &mut lock_toml)?;
        let mut opts = PublishOptions::new(intent.clone());
        opts.files = false; // the manifest is built from git objects, below
        opts.examples = state.examples;
        opts.allow_empty = true;
        match publish_core::publish_dir(&state.store_root, scratch.path(), Some(&work), opts) {
            Ok(Outcome::Published(p)) => ops += p.ops.len(),
            Ok(Outcome::DryRun(_)) => unreachable!("dry_run is never set"),
            Err(PublishError::TypeCheck(errs)) => {
                return Ok(CommitOutcome::Refused(Refusal {
                    reason: "gate:type-check",
                    phase: "type-check",
                    message: format!("type-check failed with {} error(s)", errs.len()),
                    diagnostics: errs.iter().filter_map(|e| serde_json::to_value(e).ok()).collect(),
                }));
            }
            Err(PublishError::Examples(errs)) => {
                return Ok(CommitOutcome::Refused(Refusal {
                    reason: "gate:examples",
                    phase: "examples",
                    message: format!("examples failed with {} error(s)", errs.len()),
                    diagnostics: errs.iter().filter_map(|e| serde_json::to_value(e).ok()).collect(),
                }));
            }
            Err(PublishError::Load(e)) => {
                return Ok(CommitOutcome::Refused(Refusal::new("lex:load", "load", format!("{e:#}"))));
            }
            // The store's write-time gate (or an unknown branch, IO, ...).
            Err(PublishError::Store(e)) => {
                return Ok(CommitOutcome::Refused(Refusal::new("gate:store", "store", e.to_string())));
            }
            Err(PublishError::Other(e)) => return Err(e),
        }
    }
    #[cfg(test)]
    if state.fail_after_semantic {
        state.saw_partial_head = state.store.get_branch(&work)?.and_then(|b| b.head_op).is_some();
        bail!("injected failure after the semantic ops");
    }
    // PR 5: a commit that DELETES the package (the work branch has live
    // declarations, this tree has none) must run the semantic pass on an empty
    // scratch dir with `allow_empty` so the removals are emitted. Unreachable
    // in the tip-only import: the target branch is required to be empty.

    // ── the manifest, LAST, under the same intent ──────────────────────────
    let manifest = {
        let mut read_err: Option<anyhow::Error> = None;
        let cat = &mut state.cat;
        let entries = cls.manifest.iter().map_while(|f| match cat.read(&f.oid) {
            Ok(bytes) => Some((f.path.clone(), if f.exec { MODE_EXEC } else { MODE_FILE }, bytes)),
            Err(e) => {
                read_err = Some(e);
                None
            }
        });
        let built = crate::files::manifest_from_files(&state.store, entries);
        match read_err {
            Some(e) => return Err(e),
            None => match built {
                Ok(m) => m,
                Err(e) => {
                    return Ok(CommitOutcome::Refused(Refusal::new("manifest:invalid", "manifest", format!("{e:#}"))));
                }
            },
        }
    };
    IntentLog::open(state.store.root())
        .and_then(|l| l.put(intent))
        .context("recording intent")?;
    let files_op = match crate::files::publish_manifest_if_changed(
        &state.store,
        &work,
        &manifest,
        Some(intent.intent_id.clone()),
    ) {
        Ok(r) => r.map(|(op, _)| op),
        Err(e) => return Ok(CommitOutcome::Refused(Refusal::new("gate:store", "manifest", format!("{e:#}")))),
    };
    if files_op.is_some() {
        ops += 1;
    }

    let head = state.store.get_branch(&work)?.and_then(|b| b.head_op);
    if ops == 0 {
        return Ok(CommitOutcome::Noop);
    }
    // The committed lock rides on the final head, as `lex publish` leaves it.
    if let (Some(h), Some(lock)) = (head.as_deref(), lock_toml.as_deref()) {
        state.store.set_committed_lock(h, lock)?;
    }
    Ok(CommitOutcome::Imported { ops, files_op, head })
}

/// Write `lex.toml`, `lex.lock` (when present) and `src/**` to `dir` from git
/// object bytes — the only files the loader needs.
fn materialize(state: &mut ImportState, cls: &Classified, dir: &Path, lock: &mut Option<String>) -> Result<()> {
    let wanted = cls.manifest.iter().filter(|f| {
        f.path == "lex.toml" || f.path == "lex.lock" || f.path.starts_with("src/")
    });
    for f in wanted.chain(cls.lex_sources.iter()) {
        let bytes = state.cat.read(&f.oid)?;
        if f.path == "lex.lock" {
            *lock = String::from_utf8(bytes.clone()).ok();
        }
        let dest = dir.join(&f.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes).with_context(|| format!("writing {}", dest.display()))?;
    }
    Ok(())
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

/// The result of a run, before rendering.
struct Report {
    imported: Vec<Value>,
    noop: Vec<String>,
    unsupported: Vec<Unsupported>,
    tip_sha: String,
    tip_landed: bool,
    refusal: Option<Refusal>,
    strict_violation: bool,
    head_op: Option<OpId>,
    store_branch: String,
}

pub fn cmd_import_git(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let a = parse_args(args)?;
    if looks_like_url(&a.source) {
        bail!(
            "importing from a URL is not yet supported (#892 PR5); clone it first and pass \
             the clone's path: `git clone --bare <url> repo.git && lex op import-git repo.git --head-only`"
        );
    }
    if !a.head_only {
        bail!(
            "full history import lands in PR5 (#892); pass --head-only to import the tip of the \
             branch as one snapshot"
        );
    }
    let report = run_import(&a)?;
    render(fmt, &report);
    if report.tip_landed && !report.strict_violation {
        Ok(())
    } else {
        std::process::exit(2);
    }
}

fn run_import(a: &ImportArgs) -> Result<Report> {
    let repo = PathBuf::from(&a.source);
    if !repo.is_dir() {
        bail!("{} is not a directory", repo.display());
    }
    git_line(&repo, &["rev-parse", "--git-dir"])
        .with_context(|| format!("{} is not a git repository", repo.display()))?;
    if !git_line(&repo, &["rev-parse", "--show-cdup"])?.is_empty() {
        bail!("{} is inside a repository; pass the repository root (the package root is the repo root)", repo.display());
    }
    if git_line(&repo, &["rev-parse", "--is-shallow-repository"])? == "true" {
        bail!(
            "{} is a shallow clone: the repo's identity is its root commit, which a shallow clone \
             does not have, so the imported OpIds would not converge with other imports",
            repo.display()
        );
    }

    // The tip: `--branch` (a local branch) or the branch HEAD points at.
    let git_branch = match &a.branch {
        Some(b) => b.clone(),
        None => git_line(&repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map_err(|_| {
            anyhow!("HEAD is detached or unborn in {}; pass --branch <name>", repo.display())
        })?,
    };
    let tip = git_line(
        &repo,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{git_branch}^{{commit}}")],
    )
    .map_err(|_| anyhow!("no branch `{git_branch}` in {}", repo.display()))?;
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
    if store.get_branch(&store_branch)?.and_then(|b| b.head_op).is_some() {
        bail!(
            "store branch `{store_branch}` already has history; this importer only fills an empty \
             branch (incremental import is #892 PR5) — import into a fresh one with \
             --store-branch <name>"
        );
    }

    let mut state = ImportState::open(repo, store_root.clone(), store, &store_branch, root_sha, a)?;
    let work_branch = state.work_branch.clone();

    let mut report = Report {
        imported: Vec::new(),
        noop: Vec::new(),
        unsupported: Vec::new(),
        tip_sha: tip.clone(),
        tip_landed: false,
        refusal: None,
        strict_violation: false,
        head_op: None,
        store_branch: store_branch.clone(),
    };
    let outcome = import_commit(&mut state, &tip);
    report.unsupported = std::mem::take(&mut state.unsupported);

    let finish = |state: &ImportState| delete_branch(&state.store_root, &state.store, &work_branch);
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            let _ = finish(&state);
            return Err(e);
        }
    };
    match outcome {
        // `--strict`: an unsupported path is an error — nothing lands.
        CommitOutcome::Imported { .. } | CommitOutcome::Noop if a.strict && !report.unsupported.is_empty() => {
            report.strict_violation = true;
            report.refusal = Some(Refusal::new(
                "strict:unsupported",
                "strict",
                format!("--strict: {} unsupported path(s) (symlinks/submodules) in the tree", report.unsupported.len()),
            ));
        }
        CommitOutcome::Imported { ops, files_op, head } => {
            let head = head.ok_or_else(|| anyhow!("import produced ops but no head"))?;
            // The single step that publishes the import: move the target branch
            // (absent/empty → fast-forward) onto the work branch's head.
            state.store.advance_branch_head_ff(&store_branch, &head).map_err(|e| anyhow!("{e}"))?;
            report.imported.push(json!({ "sha": tip, "ops": ops, "files_op": files_op }));
            report.head_op = Some(head);
            report.tip_landed = true;
        }
        CommitOutcome::Noop => {
            report.noop.push(tip.clone());
            report.tip_landed = true;
        }
        CommitOutcome::Refused(r) => report.refusal = Some(r),
    }
    finish(&state)?;
    Ok(report)
}

fn render(fmt: &OutputFormat, r: &Report) {
    let unsupported: Vec<Value> = r
        .unsupported
        .iter()
        .map(|u| json!({ "path": u.path, "kind": u.kind, "commit": u.commit }))
        .collect();
    let mut tip = json!({ "sha": r.tip_sha, "landed": r.tip_landed && !r.strict_violation });
    if let Some(f) = &r.refusal {
        tip["phase"] = json!(f.phase);
        tip["reason"] = json!(f.reason);
        tip["message"] = json!(f.message);
        tip["diagnostics"] = json!(f.diagnostics);
    }
    let data = json!({
        "imported": r.imported,
        "folded": [],
        "noop": r.noop,
        "unsupported": unsupported,
        "tip": tip,
        "store_branch": r.store_branch,
        "head_op": r.head_op,
        "toolchain": format!("lex {}", crate::acli::VERSION),
    });
    let text = || {
        match (&r.refusal, r.tip_landed) {
            (None, true) if r.imported.is_empty() => println!("tip {} changed nothing; nothing imported", r.tip_sha),
            (None, true) => {
                let i = &r.imported[0];
                println!(
                    "imported tip {} onto store branch `{}` ({} op(s){})",
                    r.tip_sha,
                    r.store_branch,
                    i["ops"],
                    match i["files_op"].as_str() {
                        Some(f) => format!(", files op {f}"),
                        None => String::new(),
                    }
                );
                if let Some(h) = &r.head_op {
                    println!("head: {h}");
                }
            }
            (Some(f), _) => {
                println!("tip {} did NOT land ({}: {})", r.tip_sha, f.phase, f.message);
                for d in &f.diagnostics {
                    eprintln!("{d}");
                }
            }
            (None, false) => println!("tip {} did NOT land", r.tip_sha),
        }
        for u in &r.unsupported {
            println!("unsupported {}: {} (commit {})", u.kind, u.path, u.commit);
        }
    };
    crate::acli::emit_or_text("op-import-git", data, fmt, text);
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
