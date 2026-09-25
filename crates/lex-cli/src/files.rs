//! `lex files` — the working copy's non-op-log files (#1007 PR 4): README,
//! `lex.toml`, `lex.lock`, `tests/`, CI config, images, anything besides
//! `src/**/*.lex` / `src.lex`, which the op-log itself owns.
//!
//!   lex files status  [--store DIR] [--branch NAME] [<pkgdir>]
//!   lex files commit  [--store DIR] [--branch NAME] [-m TEXT] [<pkgdir>]
//!   lex files ls      [--store DIR] [--branch NAME] [--at OP]
//!   lex files cat     [--store DIR] [--branch NAME] [--at OP] <path>
//!   lex files checkout [--store DIR] [--branch NAME] [--at OP] <dir>
//!
//! `build_manifest`/`publish_files_if_changed` are also used by `lex
//! publish` (via `crate::publish_core`) to capture a directory publish's
//! files into a `SetFiles` op — see that module for the write path this one
//! shares. The manifest pipeline is split into three reusable steps: the
//! working-copy scan ([`scan_workdir`]), `manifest_from_files` (bytes in,
//! canonical manifest out) and `publish_manifest_if_changed` (the `SetFiles`
//! write).

use super::*;
use lex_store::files::{is_reserved_path, MODE_EXEC, MODE_FILE};
use lex_store::{FileEntry, Manifest, ManifestAt};
use std::path::Path;

pub fn cmd_files(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let tail: &[String] = if args.is_empty() { args } else { &args[1..] };
    match sub {
        "status" => cmd_files_status(fmt, tail),
        "commit" => cmd_files_commit(fmt, tail),
        "ls" => cmd_files_ls(fmt, tail),
        "cat" => cmd_files_cat(fmt, tail),
        "checkout" => cmd_files_checkout(fmt, tail),
        _ => bail!(
            "usage: lex files <status|commit|ls|cat|checkout> [--store DIR] [--branch NAME]\n\
             \x20 status:   [<pkgdir>]              working copy vs. the branch head's files manifest\n\
             \x20 commit:   [-m TEXT] [<pkgdir>]     record a files-only SetFiles op (warns on pending src/ changes)\n\
             \x20 ls:       [--at OP]                list the files manifest in force\n\
             \x20 cat:      [--at OP] <path>          print one file's contents\n\
             \x20 checkout: [--at OP] <dir>           materialize src/ + manifest files into <dir>, no git"
        ),
    }
}

// ── working-copy scan ────────────────────────────────────────────────────

/// One file discovered in the working copy, before hashing.
struct ScannedFile {
    /// Manifest-relative, `/`-separated.
    rel: String,
    abs: PathBuf,
    mode: &'static str,
}

/// Enumerate `pkg_dir`'s file set + modes (#1007 §2): prefers `git ls-files
/// -z -s` inside a git worktree (the exact tracked set, including a
/// force-added file that would otherwise be gitignored); falls back to a
/// filesystem walk honoring `.gitignore` + an optional `.lexignore` when the
/// package isn't in a git worktree (or `git` isn't on PATH). Always excludes
/// `.git/`, `.lex/`, and anything [`is_reserved_path`] claims for the
/// op-log — those are skipped here rather than left to fail
/// `Manifest::validate` later. Symlinks and submodules are unsupported in
/// files-v1 (documented limitation): skipped with a note on stderr, not a
/// hard failure.
fn scan_workdir(pkg_dir: &Path) -> Result<Vec<ScannedFile>> {
    if let Some(files) = scan_git(pkg_dir)? {
        Ok(files)
    } else {
        scan_walk(pkg_dir)
    }
}

/// `git ls-files -z -s` scoped to `pkg_dir`. `Ok(None)` means "not a git
/// worktree" (or `git` isn't installed) — the caller falls back to the
/// filesystem walker. A real git failure (rare — a corrupt repo) also falls
/// back rather than hard-failing a publish over an unrelated git problem.
fn scan_git(pkg_dir: &Path) -> Result<Option<Vec<ScannedFile>>> {
    let out = match std::process::Command::new("git")
        .arg("-C")
        .arg(pkg_dir)
        .args(["ls-files", "-z", "-s"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    if !out.status.success() {
        return Ok(None);
    }
    let mut files = Vec::new();
    for raw in out.stdout.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let line = String::from_utf8_lossy(raw);
        let Some(tab) = line.find('\t') else { continue };
        let meta = &line[..tab];
        let rel = line[tab + 1..].to_string();
        let mode = match meta.split_whitespace().next().unwrap_or("") {
            "100644" => MODE_FILE,
            "100755" => MODE_EXEC,
            "120000" => {
                eprintln!("lex files: skipping symlink `{rel}` (unsupported in files-v1)");
                continue;
            }
            "160000" => {
                eprintln!("lex files: skipping submodule `{rel}` (unsupported in files-v1)");
                continue;
            }
            _ => continue,
        };
        if is_reserved_path(&rel) || under_dotgit_or_dotlex(&rel) {
            continue;
        }
        files.push(ScannedFile { abs: pkg_dir.join(&rel), rel, mode });
    }
    Ok(Some(files))
}

/// Filesystem walk honoring `.gitignore` + `.lexignore`, for a package not
/// in a git worktree. `require_git(false)` makes `.gitignore` apply even
/// with no actual `.git` directory present, matching what a later `git
/// init` in the same directory would track.
fn scan_walk(pkg_dir: &Path) -> Result<Vec<ScannedFile>> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(pkg_dir)
        .hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".lexignore")
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        if entry.path() == pkg_dir {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(pkg_dir) else { continue };
        let rel_str = path_to_manifest_str(rel);
        if under_dotgit_or_dotlex(&rel_str) {
            continue;
        }
        let Some(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            continue;
        }
        if !ft.is_file() {
            eprintln!(
                "lex files: skipping non-regular entry `{rel_str}` (symlinks unsupported in files-v1)"
            );
            continue;
        }
        if is_reserved_path(&rel_str) {
            continue;
        }
        let mode = exec_mode(entry.path());
        files.push(ScannedFile { abs: entry.path().to_path_buf(), rel: rel_str, mode });
    }
    Ok(files)
}

/// Every regular file under `pkg_dir`, ignore rules bypassed entirely
/// (still skipping `.git/`/`.lex/`) — used only by `status` to report which
/// on-disk files are being excluded by an ignore rule.
fn all_disk_files(pkg_dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(pkg_dir)
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .build();
    for result in walker {
        let Ok(entry) = result else { continue };
        if entry.path() == pkg_dir {
            continue;
        }
        let Some(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(pkg_dir) else { continue };
        let rel_str = path_to_manifest_str(rel);
        if under_dotgit_or_dotlex(&rel_str) {
            continue;
        }
        out.push(rel_str);
    }
    Ok(out)
}

fn under_dotgit_or_dotlex(rel: &str) -> bool {
    rel.split('/')
        .next()
        .map(|c| c.eq_ignore_ascii_case(".git") || c.eq_ignore_ascii_case(".lex"))
        .unwrap_or(false)
}

fn path_to_manifest_str(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(unix)]
fn exec_mode(p: &Path) -> &'static str {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) if m.permissions().mode() & 0o111 != 0 => MODE_EXEC,
        _ => MODE_FILE,
    }
}
#[cfg(not(unix))]
fn exec_mode(_p: &Path) -> &'static str {
    MODE_FILE
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

// ── manifest building + the shared write path ───────────────────────────

/// Build the canonical files [`Manifest`] from in-memory `(path, mode, bytes)`
/// entries: put each file's bytes as a blob, then assemble and validate the
/// manifest (`Manifest::validate` — the #1007 path/mode/size rules). `path` is
/// manifest-relative and `/`-separated; `mode` is `MODE_FILE` (`100644`) or
/// `MODE_EXEC` (`100755`).
///
/// This takes bytes, not a directory, so a caller with no checkout (`lex op
/// import-git` reading git objects) produces exactly the manifest a working
/// copy holding the same bytes would. It does **not** filter: reserved
/// `src/**/*.lex` / `src.lex` paths and `.git/`/`.lex/` entries are the
/// *scan's* job ([`scan_workdir`]) and a caller feeding entries directly must
/// exclude them itself — `validate` rejects a reserved path rather than
/// silently dropping it.
pub(crate) fn manifest_from_files(
    store: &Store,
    files: impl IntoIterator<Item = (String, &'static str, Vec<u8>)>,
) -> Result<Manifest> {
    let mut m = Manifest::new();
    for (rel, mode, bytes) in files {
        let size = bytes.len() as u64;
        let blob = store
            .put_blob_bytes(&bytes)
            .map_err(|e| anyhow!("storing blob for `{rel}`: {e}"))?;
        m.entries.insert(rel, FileEntry { blob, mode: mode.to_string(), size });
    }
    m.validate().map_err(|e| anyhow!("building files manifest: {e}"))?;
    Ok(m)
}

/// Build the files manifest for `pkg_dir`'s working copy: scan the file
/// set ([`scan_workdir`]), hash and store each file as a blob, then
/// assemble and validate the resulting [`Manifest`]. Every reserved
/// `src/**/*.lex` / `src.lex` path is excluded by the scan before it can
/// ever reach `Manifest::validate` — the op-log owns those, so even a file
/// that happens to be tracked at such a path (accidentally or otherwise)
/// never makes it into the files manifest.
pub(crate) fn build_manifest(store: &Store, pkg_dir: &Path) -> Result<Manifest> {
    let scanned = scan_workdir(pkg_dir)?;
    // Stream one file at a time (a working copy can hold large images): read
    // lazily as `manifest_from_files` consumes the iterator, and surface the
    // first read failure ahead of any manifest error it may have caused.
    let mut read_err: Option<anyhow::Error> = None;
    let entries = scanned.into_iter().map_while(|f| match std::fs::read(&f.abs) {
        Ok(bytes) => Some((f.rel, f.mode, bytes)),
        Err(e) => {
            read_err = Some(anyhow::Error::new(e).context(format!("reading {}", f.abs.display())));
            None
        }
    });
    let built = manifest_from_files(store, entries);
    match read_err {
        Some(e) => Err(e),
        None => built,
    }
}

/// If `manifest` differs from `branch`'s current files manifest, store it and
/// append exactly one `SetFiles` op — parented on `branch`'s CURRENT head at
/// call time, so a caller that already applied semantic ops on this branch
/// (`lex publish`, the import path) gets the `SetFiles` chained after them for
/// free. Returns `None` (and writes nothing beyond the content-addressed blobs
/// already put while building the manifest, which is idempotent) when nothing
/// non-semantic changed.
pub(crate) fn publish_manifest_if_changed(
    store: &Store,
    branch: &str,
    manifest: &Manifest,
    intent_id: Option<lex_vcs::IntentId>,
) -> Result<Option<(lex_vcs::OpId, lex_store::BlobId)>> {
    let new_id = manifest.id();
    let current = store
        .branch_manifest(branch)
        .map_err(|e| anyhow!("reading current files manifest: {e}"))?;
    if current.manifest() == Some(&new_id) {
        return Ok(None);
    }
    let manifest_id = store
        .put_manifest(manifest)
        .map_err(|e| anyhow!("storing files manifest: {e}"))?;
    let op_id = store
        .apply_set_files(branch, &manifest_id, intent_id.as_ref())
        .map_err(|e| anyhow!("recording files manifest: {e}"))?;
    Ok(Some((op_id, manifest_id)))
}

/// If `build_manifest(pkg_dir)` differs from `branch`'s current files
/// manifest, store it and append exactly one `SetFiles` op (see
/// [`publish_manifest_if_changed`]). Returns `None` when nothing
/// non-semantic changed.
pub(crate) fn publish_files_if_changed(
    store: &Store,
    branch: &str,
    pkg_dir: &Path,
    intent_id: Option<lex_vcs::IntentId>,
) -> Result<Option<(lex_vcs::OpId, lex_store::BlobId)>> {
    let manifest = build_manifest(store, pkg_dir)?;
    publish_manifest_if_changed(store, branch, &manifest, intent_id)
}

/// Best-effort: whether `pkg_dir`/src differs semantically from `branch`'s
/// head. Used only to warn `lex files commit` that it is about to record a
/// files-only snapshot while code changes sit unpublished — any read/parse
/// error reads as "can't tell" (`false`), since this is advisory, not a
/// gate `lex files commit` should ever refuse over.
fn semantic_changes_pending(store: &Store, branch: &str, pkg_dir: &Path) -> bool {
    let Some(path_str) = pkg_dir.to_str() else { return false };
    let Ok((prog, _imports, _prefixes)) = crate::store::read_publish_source(path_str, false)
    else {
        return false;
    };
    let stages = lex_ast::canonicalize_program(&prog);
    let Ok(old_head) = store.branch_head(branch) else { return false };
    let head_pairs: Vec<(String, String)> =
        old_head.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let mut old_fns: BTreeMap<String, lex_ast::FnDecl> = BTreeMap::new();
    let mut old_types: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    for ast in store.get_asts_for_sigs_bulk(&head_pairs).into_iter().filter_map(|r| r.ok()) {
        match ast {
            Stage::FnDecl(fd) => {
                old_fns.insert(fd.name.clone(), fd);
            }
            Stage::TypeDecl(td) => {
                old_types.insert(td.name.clone(), td);
            }
            _ => {}
        }
    }
    let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|s| match s {
            Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let new_types: BTreeMap<String, lex_ast::TypeDecl> = stages
        .iter()
        .filter_map(|s| match s {
            Stage::TypeDecl(td) => Some((td.name.clone(), td.clone())),
            _ => None,
        })
        .collect();
    let report =
        lex_vcs::compute_diff_with_types(&old_fns, &new_fns, &old_types, &new_types, false);
    !(report.added.is_empty()
        && report.removed.is_empty()
        && report.renamed.is_empty()
        && report.modified.is_empty())
}

// ── flag parsing shared by the subcommands ───────────────────────────────

fn open_store_and_branch(root: &Path, branch: Option<String>) -> Result<(Store, String)> {
    let store =
        Store::open(root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    Ok((store, branch))
}

fn resolve_manifest_at(store: &Store, branch: &str, at: Option<&str>) -> Result<ManifestAt> {
    match at {
        Some(op) => store.manifest_at(op).map_err(|e| anyhow!("{e}")),
        None => store.branch_manifest(branch).map_err(|e| anyhow!("{e}")),
    }
}

fn manifest_entries(store: &Store, m: &ManifestAt) -> Result<Vec<(String, FileEntry)>> {
    match m.manifest() {
        Some(id) => Ok(store
            .get_manifest(id)
            .map_err(|e| anyhow!("{e}"))?
            .entries
            .into_iter()
            .collect()),
        None => {
            if matches!(m, ManifestAt::Ambiguous) {
                bail!(
                    "the files manifest is ambiguous at this op (a merge whose parents carry \
                     different manifests, with no SetFiles on top yet) — pass --at a SetFiles \
                     op, or a head where the merge's own SetFiles has landed"
                );
            }
            Ok(Vec::new())
        }
    }
}

// ── status ────────────────────────────────────────────────────────────────

#[derive(Default, serde::Serialize)]
struct FileStatus {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    ignored: Vec<String>,
}

fn compute_status(store: &Store, branch: &str, pkg_dir: &Path) -> Result<FileStatus> {
    let scanned = scan_workdir(pkg_dir)?;
    let manifest_at = store
        .branch_manifest(branch)
        .map_err(|e| anyhow!("reading files manifest: {e}"))?;
    let old_entries: BTreeMap<String, FileEntry> = match manifest_at.manifest() {
        Some(id) => store.get_manifest(id).map_err(|e| anyhow!("{e}"))?.entries,
        None => BTreeMap::new(),
    };

    let mut st = FileStatus::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for f in &scanned {
        seen.insert(f.rel.clone());
        let bytes = std::fs::read(&f.abs).with_context(|| format!("reading {}", f.abs.display()))?;
        let hash = sha256_hex(&bytes);
        match old_entries.get(&f.rel) {
            None => st.added.push(f.rel.clone()),
            Some(e) if e.blob != hash || e.mode != f.mode => st.modified.push(f.rel.clone()),
            Some(_) => {}
        }
    }
    for path in old_entries.keys() {
        if !seen.contains(path) {
            st.deleted.push(path.clone());
        }
    }
    for p in all_disk_files(pkg_dir)? {
        if seen.contains(&p) || old_entries.contains_key(&p) || is_reserved_path(&p) {
            continue;
        }
        st.ignored.push(p);
    }
    st.added.sort();
    st.modified.sort();
    st.deleted.sort();
    st.ignored.sort();
    Ok(st)
}

fn cmd_files_status(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let mut branch: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else {
            positional.push(a.clone());
        }
    }
    let pkg_dir = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (store, branch) = open_store_and_branch(&root, branch)?;

    let st = compute_status(&store, &branch, &pkg_dir)?;
    let data = serde_json::json!({
        "branch": branch,
        "added": st.added,
        "modified": st.modified,
        "deleted": st.deleted,
        "ignored": st.ignored,
    });
    acli::emit_or_text("files-status", data, fmt, || {
        for p in &st.added {
            println!("added     {p}");
        }
        for p in &st.modified {
            println!("modified  {p}");
        }
        for p in &st.deleted {
            println!("deleted   {p}");
        }
        for p in &st.ignored {
            println!("ignored   {p}");
        }
        if st.added.is_empty() && st.modified.is_empty() && st.deleted.is_empty() {
            println!("working copy matches the files manifest at {branch}'s head");
        }
    });
    Ok(())
}

// ── commit ────────────────────────────────────────────────────────────────

fn cmd_files_commit(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let mut branch: Option<String> = None;
    let mut intent_prompt: Option<String> = None;
    let mut intent_model: Option<String> = None;
    let mut intent_session: Option<String> = None;
    let mut intent_issue: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone())
            }
            "-m" | "--intent-prompt" => {
                intent_prompt =
                    Some(it.next().ok_or_else(|| anyhow!("{a} needs a value"))?.clone())
            }
            "--intent-model" => {
                intent_model = Some(
                    it.next().ok_or_else(|| anyhow!("--intent-model needs a value"))?.clone(),
                )
            }
            "--intent-session" => {
                intent_session = Some(
                    it.next().ok_or_else(|| anyhow!("--intent-session needs a value"))?.clone(),
                )
            }
            "--intent-issue" => {
                intent_issue = Some(
                    it.next().ok_or_else(|| anyhow!("--intent-issue needs a value"))?.clone(),
                )
            }
            other => positional.push(other.to_string()),
        }
    }
    let pkg_dir = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (store, branch) = open_store_and_branch(&root, branch)?;

    // Advisory only (#1007 PR 4 scope): `lex files commit` records a
    // files-only snapshot. It does not publish code — warn, don't refuse,
    // when src/ has changes that aren't in the branch head yet, so a caller
    // doesn't mistake "committed the files" for "published everything".
    if semantic_changes_pending(&store, &branch, &pkg_dir) {
        eprintln!(
            "warning: {}/src has unpublished semantic changes -- `lex files commit` only \
             records files, not code; run `lex publish` to publish them too",
            pkg_dir.display()
        );
    }

    let manifest = build_manifest(&store, &pkg_dir)?;
    let new_id = manifest.id();
    let current = store
        .branch_manifest(&branch)
        .map_err(|e| anyhow!("reading current files manifest: {e}"))?;
    if current.manifest() == Some(&new_id) {
        let head_op = store.get_branch(&branch).ok().flatten().and_then(|b| b.head_op);
        let data = serde_json::json!({
            "ops": Vec::<serde_json::Value>::new(),
            "head_op": head_op,
        });
        acli::emit_or_text("files-commit", data, fmt, || {
            println!("no files changes to commit");
        });
        return Ok(());
    }

    let intent_id =
        crate::store::record_intent(&root, intent_prompt, intent_model, intent_session, intent_issue)?;
    let manifest_id =
        store.put_manifest(&manifest).map_err(|e| anyhow!("storing files manifest: {e}"))?;
    let op_id = store
        .apply_set_files(&branch, &manifest_id, intent_id.as_ref())
        .map_err(|e| anyhow!("recording files manifest: {e}"))?;

    let data = serde_json::json!({
        "ops": [{
            "op_id": op_id,
            "kind": serde_json::to_value(&lex_vcs::OperationKind::SetFiles {
                manifest: manifest_id.clone(),
            }).expect("SetFiles serializes"),
        }],
        "head_op": op_id,
        "intent_id": intent_id,
        "files_manifest": manifest_id,
    });
    acli::emit_or_text("files-commit", data, fmt, || {
        println!("committed files manifest {manifest_id} as {op_id}");
    });
    Ok(())
}

// ── ls ────────────────────────────────────────────────────────────────────

fn cmd_files_ls(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let mut branch: Option<String> = None;
    let mut at: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone())
            }
            "--at" => at = Some(it.next().ok_or_else(|| anyhow!("--at needs an op id"))?.clone()),
            other => bail!("unknown flag `{other}` for `lex files ls`"),
        }
    }
    let (store, branch) = open_store_and_branch(&root, branch)?;
    let manifest_at = resolve_manifest_at(&store, &branch, at.as_deref())?;
    let mut entries = manifest_entries(&store, &manifest_at)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let data = serde_json::json!({
        "entries": entries.iter().map(|(p, e)| serde_json::json!({
            "path": p, "blob": e.blob, "mode": e.mode, "size": e.size,
        })).collect::<Vec<_>>(),
    });
    acli::emit_or_text("files-ls", data, fmt, || {
        for (path, e) in &entries {
            println!("{}\t{}\t{}\t{}", e.mode, e.size, e.blob, path);
        }
    });
    Ok(())
}

// ── cat ───────────────────────────────────────────────────────────────────

fn cmd_files_cat(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let mut branch: Option<String> = None;
    let mut at: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone())
            }
            "--at" => at = Some(it.next().ok_or_else(|| anyhow!("--at needs an op id"))?.clone()),
            other => positional.push(other.to_string()),
        }
    }
    let path = positional
        .first()
        .ok_or_else(|| anyhow!("usage: lex files cat [--at OP] <path>"))?
        .clone();
    let (store, branch) = open_store_and_branch(&root, branch)?;
    let manifest_at = resolve_manifest_at(&store, &branch, at.as_deref())?;
    let entries = manifest_entries(&store, &manifest_at)?;
    let entry = entries
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, e)| e.clone())
        .ok_or_else(|| anyhow!("no such file `{path}` in the files manifest"))?;
    let bytes = store.get_blob_bytes(&entry.blob).map_err(|e| anyhow!("{e}"))?;

    if matches!(fmt, OutputFormat::Json) {
        use base64::Engine as _;
        let data = serde_json::json!({
            "path": path,
            "blob": entry.blob,
            "mode": entry.mode,
            "size": entry.size,
            "content_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
        acli::emit_or_text("files-cat", data, fmt, || {});
    } else {
        use std::io::Write as _;
        std::io::stdout().write_all(&bytes)?;
    }
    Ok(())
}

// ── checkout ──────────────────────────────────────────────────────────────

fn cmd_files_checkout(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let mut branch: Option<String> = None;
    let mut at: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone())
            }
            "--at" => at = Some(it.next().ok_or_else(|| anyhow!("--at needs an op id"))?.clone()),
            other => positional.push(other.to_string()),
        }
    }
    let out_dir = positional
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: lex files checkout [--at OP] <dir>"))?;
    let (store, branch) = open_store_and_branch(&root, branch)?;
    let head_op = match at {
        Some(op) => op,
        None => store
            .get_branch(&branch)
            .map_err(|e| anyhow!("{e}"))?
            .and_then(|b| b.head_op)
            .ok_or_else(|| anyhow!("branch `{branch}` has no head to check out"))?,
    };

    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating {}", out_dir.display()))?;

    // src/ (or src.lex) — rendered from the op-log; no git, no working
    // directory involved (lex-code needs exactly this to materialize a
    // package from a store alone).
    let head = lex_store::render::package_head_at_op(&store, &head_op).map_err(|e| anyhow!("{e}"))?;
    let rendered = lex_store::render::render_source(&store, &head).map_err(|e| anyhow!("{e}"))?;
    let src_files: Vec<(String, String)> = match rendered {
        lex_store::render::RenderedSource::Single { path, src } => {
            vec![(path.unwrap_or_else(|| "src.lex".to_string()), src)]
        }
        lex_store::render::RenderedSource::Multi(tree) => tree.into_iter().collect(),
    };
    let mut written = 0usize;
    for (relpath, src) in &src_files {
        let p = out_dir.join(relpath);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, src).with_context(|| format!("writing {}", p.display()))?;
        written += 1;
    }

    // The files manifest at that op: README, lex.toml, lex.lock, tests/, ...
    let manifest_at = store.manifest_at(&head_op).map_err(|e| anyhow!("{e}"))?;
    let mut manifest_files = 0usize;
    match manifest_at.manifest() {
        Some(id) => {
            let m = store.get_manifest(id).map_err(|e| anyhow!("{e}"))?;
            for (path, entry) in &m.entries {
                let bytes = store.get_blob_bytes(&entry.blob).map_err(|e| anyhow!("{e}"))?;
                let p = out_dir.join(path);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, &bytes).with_context(|| format!("writing {}", p.display()))?;
                #[cfg(unix)]
                if entry.mode == MODE_EXEC {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perm = std::fs::metadata(&p)?.permissions();
                    perm.set_mode(perm.mode() | 0o111);
                    std::fs::set_permissions(&p, perm)?;
                }
                manifest_files += 1;
            }
        }
        None if matches!(manifest_at, ManifestAt::Ambiguous) => {
            eprintln!(
                "warning: the files manifest is ambiguous at {head_op} (an unmerged merge \
                 point) -- src/ was checked out, but non-op-log files were not"
            );
        }
        None => {}
    }

    let data = serde_json::json!({
        "dir": out_dir.display().to_string(),
        "at": head_op,
        "src_files": written,
        "manifest_files": manifest_files,
    });
    acli::emit_or_text("files-checkout", data, fmt, || {
        println!(
            "checked out {written} src file(s) + {manifest_files} manifest file(s) into {}",
            out_dir.display()
        );
    });
    Ok(())
}
