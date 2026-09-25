//! `lex export-git` — render a branch's op history as a git repository
//! (#837, Tier 3: the interop half of "source of record").
//!
//! The op log is the canonical history; git is a *view* of it. This
//! walks a branch oldest→newest, replays each op into a running
//! SigId→StageId head map, renders the resulting stages back to source
//! with the canonical printer, and lands one git commit per op — the
//! op's intent prompt as the message (falling back to a kind summary),
//! the op id as a trailer. The result is a repo humans, GitHub, and
//! IDEs can read, reconstructed deterministically from the typed log.
//!
//! Rendering (single `src.lex` for a single module, or the de-flattened
//! `src/*.lex` tree for a multi-module package, #894) is done by the shared
//! `lex_store::render`, so the git view is byte-identical to the source a
//! consumer installs from the hosted registry. A git-import path (text diff →
//! typed ops via `diff_to_ops`) is a follow-up on #837.
//!
//! **Origin-aware (#892 PR 3).** An op whose intent carries an
//! [`Origin`] (it was imported from a git commit) is exported as that commit
//! again, not as a synthetic one: the commit message is the intent's prompt
//! verbatim, the author and committer identities and dates come from the
//! origin, a `Git-Source:` trailer names the source commit, and the run of
//! consecutive ops that share the intent (one imported commit yields many
//! ops) collapses into ONE git commit. Everything above is gated on
//! `origin.is_some()`: a store with no origin-bearing intent exports
//! byte-identically to before.
//!
//! Usage:
//!   lex export-git <out_dir> [--branch NAME] [--store DIR]

use super::*;
use lex_store::files::MODE_EXEC;
use lex_store::{FileEntry, Manifest};
use lex_vcs::{Intent, IntentLog, OpLog, OperationKind, Origin, Person, StageTransition};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub fn cmd_export_git(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut out_dir: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--branch" => { branch = args.get(i + 1).cloned(); i += 2; }
            "--store" => { store_root = args.get(i + 1).map(PathBuf::from); i += 2; }
            other if !other.starts_with("--") && out_dir.is_none() => {
                out_dir = Some(PathBuf::from(other)); i += 1;
            }
            other => bail!("unexpected arg `{other}` (usage: lex export-git <out_dir> [--branch NAME] [--store DIR])"),
        }
    }
    let out_dir = out_dir.ok_or_else(|| anyhow!("usage: lex export-git <out_dir> [--branch NAME] [--store DIR]"))?;
    let root = store_root.unwrap_or_else(default_store_root);
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    let head = store
        .get_branch(&branch)?
        .and_then(|b| b.head_op);
    let log = OpLog::open(&root).with_context(|| "opening op log")?;
    let intents = IntentLog::open(&root).with_context(|| "opening intent log")?;

    let records = match &head {
        Some(h) => log.walk_forward(h, None)?,
        None => Vec::new(),
    };

    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    if !out_dir.join(".git").exists() {
        run_git(&out_dir, &["init", "-q"])?;
    }
    // Deterministic identity so re-exporting the same log is stable.
    run_git(&out_dir, &["config", "user.name", "lex-export"])?;
    run_git(&out_dir, &["config", "user.email", "lex-export@localhost"])?;

    let src_path = out_dir.join("src.lex");
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    // Imports live outside the SigId→StageId head map (they replay as
    // `ImportOnly`, a no-op there), so track them from the op kinds
    // directly. `flat_imports` is `reference` → `alias` for the
    // single-file render (#895); `file_imports` is the same per source
    // file, for the multi-file render (#894 slice 2b).
    let mut head_imports = lex_store::render::PackageHead::default();
    // SigId → the source file its declaration came from (from each
    // AddFunction/AddType's `in_file`). When every head stage has one, the
    // package was published multi-module and we de-flatten it back into a
    // `src/*.lex` tree; otherwise we render one `src.lex`.
    let mut sig_files: BTreeMap<String, String> = BTreeMap::new();
    // The files manifest in force (#1007 PR 6): starts empty (a store with
    // no `SetFiles` op — the pre-#1007 shape — never touches it, so such a
    // store exports byte-identically to the pre-#1007 renderer). Updated
    // only by a `SetFiles` op; carried forward unchanged otherwise, exactly
    // like `Store::manifest_at`'s single-parent inheritance.
    let mut manifest = Manifest::new();
    let mut commits = 0usize;

    // Each record's intent origin (#892 PR 3), resolved once up front. `None`
    // for an op with no intent, an intent missing from the log, or a native
    // intent — all of which take the pre-#892 one-commit-per-op path.
    // Holds the intent id when (and only when) that intent has an origin, and
    // the origin-bearing intents themselves, once each.
    let mut origin_intents: BTreeMap<String, Intent> = BTreeMap::new();
    let mut origin_ids: Vec<Option<String>> = Vec::with_capacity(records.len());
    for rec in &records {
        let mut id = None;
        if let Some(intent_id) = &rec.op.intent_id {
            if origin_intents.contains_key(intent_id) {
                id = Some(intent_id.clone());
            } else if let Some(i) = intents.get(intent_id)? {
                if i.origin.is_some() {
                    origin_intents.insert(intent_id.clone(), i);
                    id = Some(intent_id.clone());
                }
            }
        }
        origin_ids.push(id);
    }

    // The manifest as of the last landed commit: the base every commit's
    // on-disk delta (only touch disk for a path that actually changed, §7
    // fidelity plan) is taken against. For a one-op commit this is the
    // manifest before that op, exactly as before; for a grouped commit it is
    // the manifest before the group's first op.
    let mut committed_manifest = Manifest::new();
    // Ops folded into the commit being assembled, and the last `SetFiles`
    // manifest among them (`Some` only when one is, for the `Files:` trailer).
    let mut group_ops = 0usize;
    let mut files_manifest_id: Option<String> = None;

    for (idx, rec) in records.iter().enumerate() {
        group_ops += 1;

        apply_transition(&mut map, &rec.produces);
        match &rec.op.kind {
            OperationKind::AddFunction { sig_id, in_file: Some(f), .. }
            | OperationKind::AddType { sig_id, in_file: Some(f), .. } => {
                sig_files.insert(sig_id.clone(), f.clone());
            }
            OperationKind::AddImport { in_file, module, alias } => {
                // The op omits the alias when it's the module's default;
                // `PackageHead` reconstructs it the same way the store does,
                // and keeps a local import (#909) out of the flat map.
                head_imports.add_import(in_file, module, alias.as_deref());
            }
            OperationKind::RemoveImport { in_file, module } => {
                head_imports.remove_import(in_file, module);
            }
            OperationKind::RenameSymbol { from, to, .. } => {
                if let Some(f) = sig_files.remove(from) {
                    sig_files.insert(to.clone(), f);
                }
            }
            OperationKind::SetFiles { manifest: manifest_id } => {
                manifest = store.get_manifest(manifest_id).map_err(|e| anyhow!("{e}"))?;
                files_manifest_id = Some(manifest_id.clone());
            }
            _ => {}
        }

        // Ops that share an origin-bearing intent are ONE git commit: keep
        // replaying (the state above is cumulative) and land it after the
        // group's last op. Only CONSECUTIVE ops group; a native op never does.
        if continues_group(&origin_ids, idx) {
            continue;
        }

        // Start each commit from a clean tree so a stage moving files, or a
        // file emptying out, is reflected (git add -A then picks up the net
        // change). Cheap relative to the op replay itself.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_dir_all(out_dir.join("src"));

        // Render the head via the shared `lex-store` renderer — the same code
        // the hosted registry's archive endpoint uses, so the git view and an
        // installed package are byte-identical source (#894).
        let head = lex_store::render::PackageHead {
            map: map.clone(),
            sig_files: sig_files.clone(),
            flat_imports: head_imports.flat_imports.clone(),
            file_imports: head_imports.file_imports.clone(),
        };
        // #988: the single-module arm now carries its own path, so the mirror
        // keeps a package's real module name instead of renaming it to `lib`.
        let tree: Vec<(String, String)> = match lex_store::render::render_source(&store, &head)? {
            // The mirror's own convention for a head that records no file:
            // `src.lex` at the repo root, as it has always been (#988).
            lex_store::render::RenderedSource::Single { path, src } => {
                vec![(path.unwrap_or_else(|| "src.lex".to_string()), src)]
            }
            lex_store::render::RenderedSource::Multi(tree) => tree.into_iter().collect(),
        };
        // #892 PR 4: a manifest-only import (a non-Lex repo) has NO declarations,
        // so it has no source tree to render: skip the synthetic empty `src.lex`
        // the renderer would otherwise add, or the export could never be
        // byte-identical to the imported tree. Gated on the commit being an
        // origin-bearing one, so a native store exports exactly as before.
        let tree = if origin_ids[idx].is_some() && map.is_empty() { Vec::new() } else { tree };
        for (relpath, src) in tree {
            let path = out_dir.join(&relpath);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, src)
                .with_context(|| format!("writing {}", path.display()))?;
        }

        // The `remove_dir_all(src/)` above just wiped out any manifest file
        // that happens to live *under* src/ (a non-`.lex` file nested there
        // — only `src/**/*.lex` is op-log-reserved, so e.g. `src/data.bin`
        // is a legal manifest path). Re-materialize those unconditionally,
        // from the manifest now in force, *after* the clean — never before,
        // or the wipe would take them right back out. This is the fix for
        // the bug the design flagged: a naive per-commit full-manifest
        // checkout done before the wipe loses anything nested under src/.
        for (path, entry) in manifest.entries.iter().filter(|(p, _)| p.starts_with("src/")) {
            write_manifest_entry(&store, &out_dir, path, entry)?;
        }
        // Everything else in the manifest lives outside src/, so it
        // survived the wipe untouched — only touch disk for a path that
        // actually changed between this commit and the last (removal,
        // write, or chmod), per the §7 fidelity plan.
        apply_manifest_diff(&store, &out_dir, &committed_manifest, &manifest)?;
        committed_manifest = manifest.clone();

        // Commit message: the intent prompt, else a kind summary.
        let origin_intent = origin_ids[idx].as_ref().map(|id| &origin_intents[id]);
        let msg = match origin_intent {
            Some(i) => origin_message(rec, i, group_ops, files_manifest_id.as_deref()),
            None => commit_message(&intents, rec, files_manifest_id.as_deref())?,
        };
        group_ops = 0;
        files_manifest_id = None;
        // `-f`: a manifest-captured file can be a *force-added* one in the
        // source repo (tracked despite matching a `.gitignore` pattern —
        // the manifest doesn't know or care why a path was captured, only
        // that `git ls-files` said it was tracked). The exported repo gets
        // its own copy of that same `.gitignore` as a manifest entry, so a
        // plain `git add -A` here would silently drop the file again on
        // every re-render. `-A` already covers deletions; `-f` just stops
        // gitignore from re-filtering what the manifest already decided.
        run_git(&out_dir, &["add", "-A", "-f"])?;
        // --allow-empty: an ImportOnly op (or a no-op transition)
        // doesn't change the tree, but the commit still records the op.
        match origin_intent.and_then(|i| i.origin.as_ref()) {
            Some(o) => commit_as_origin(&out_dir, &msg, o)?,
            None => run_git(&out_dir, &["commit", "-q", "--allow-empty", "-m", &msg])?,
        }
        commits += 1;
    }

    let data = serde_json::json!({
        "out_dir": out_dir.display().to_string(),
        "branch": branch,
        "commits": commits,
    });
    let out_for_text = out_dir.display().to_string();
    let branch_for_text = branch.clone();
    acli::emit_or_text("export-git", data, fmt, move || {
        println!("exported {commits} commit(s) from branch {branch_for_text} to {out_for_text}");
    });
    Ok(())
}


/// One op → one commit message. The intent's prompt is the actual
/// causal event (a commit message can be made up; the prompt is what
/// happened), so prefer it; fall back to the op kind. The op id goes in
/// a trailer so the git view is traceable back to the log. A `SetFiles`
/// op carries an intent like any other (#1007), so it picks up the same
/// prompt-as-subject convention; `files_manifest` (its own manifest id,
/// `Some` only for a `SetFiles` op) adds a `Files:` trailer.
fn commit_message(
    intents: &IntentLog,
    rec: &lex_vcs::OperationRecord,
    files_manifest: Option<&str>,
) -> Result<String> {
    let subject = match &rec.op.intent_id {
        Some(id) => intents
            .get(id)?
            .map(|i| first_line(&i.prompt))
            .unwrap_or_else(|| kind_summary(&rec.op.kind)),
        None => kind_summary(&rec.op.kind),
    };
    let intent_line = rec
        .op
        .intent_id
        .as_ref()
        .map(|id| format!("\nIntent: {id}"))
        .unwrap_or_default();
    let files_line = files_manifest
        .map(|m| format!("\nFiles: {m}"))
        .unwrap_or_default();
    Ok(format!("{subject}\n\nOp: {}{intent_line}{files_line}", rec.op_id))
}

/// True when record `idx` is followed by another op of the same
/// origin-bearing intent, i.e. its git commit is not landed yet. Only the
/// IMMEDIATELY next record counts: an intent that reappears after a
/// different one starts a new commit.
fn continues_group(origin_ids: &[Option<String>], idx: usize) -> bool {
    origin_ids[idx].is_some() && origin_ids.get(idx + 1) == Some(&origin_ids[idx])
}

/// The commit message of an origin-bearing commit (#892): the intent's prompt
/// VERBATIM, then a blank line, then the trailer block — `Op:` (the LAST op
/// of the group), `Intent:`, `Files:` (when the group holds a `SetFiles`),
/// `Git-Source:` (the source commit) and `Ops:` (how many ops it folds).
///
/// Only trailing newlines of the prompt are dropped, so the separator is
/// exactly one blank line. That blank line is what keeps the verbatim body
/// and the trailers apart for `git interpret-trailers`: git parses trailers
/// from the LAST paragraph only, so a message that itself ends in
/// `Signed-off-by:` / `Op: fake` lines stays a separate, earlier paragraph
/// and cannot be mistaken for — or merge into — ours. (Written with
/// `--cleanup=verbatim`, see [`commit_as_origin`], so git does not
/// re-normalize the body.)
fn origin_message(
    rec: &lex_vcs::OperationRecord,
    intent: &Intent,
    ops: usize,
    files_manifest: Option<&str>,
) -> String {
    let body = intent.prompt.trim_end_matches(['\n', '\r']);
    // A blank prompt cannot head a trailer block; keep today's placeholder.
    let body = if body.trim().is_empty() { "(empty prompt)" } else { body };
    let commit = intent.origin.as_ref().map(|o| o.commit.as_str()).unwrap_or_default();
    let mut msg = format!("{body}\n\nOp: {}\nIntent: {}", rec.op_id, intent.intent_id);
    if let Some(m) = files_manifest {
        msg.push_str(&format!("\nFiles: {m}"));
    }
    msg.push_str(&format!("\nGit-Source: {commit}\nOps: {ops}\n"));
    msg
}

/// Land the staged tree as one commit carrying the origin's identities and
/// dates (#892). The identity and dates are set through the environment, which
/// outranks every git config, so the local `user.*` never leaks in; the
/// committer falls back to the author when the source recorded no distinct
/// one. Signing (`commit.gpgsign`), hooks and the message cleanup mode are
/// pinned off so the user's own git config can neither prompt, fail nor
/// rewrite the verbatim message.
fn commit_as_origin(dir: &Path, msg: &str, origin: &Origin) -> Result<()> {
    use std::io::Write;
    let author = &origin.author;
    let committer = origin.committer.as_ref().unwrap_or(author);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["commit", "-q", "--allow-empty", "--no-verify", "--no-gpg-sign"])
        .args(["--cleanup=verbatim", "-F", "-"])
        .env("GIT_AUTHOR_NAME", ident_name(author))
        .env("GIT_AUTHOR_EMAIL", &author.email)
        .env("GIT_AUTHOR_DATE", git_date(author))
        .env("GIT_COMMITTER_NAME", ident_name(committer))
        .env("GIT_COMMITTER_EMAIL", &committer.email)
        .env("GIT_COMMITTER_DATE", git_date(committer))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running git commit")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(msg.as_bytes())
        .context("writing the commit message to git")?;
    let out = child.wait_with_output().context("waiting for git commit")?;
    if !out.status.success() {
        bail!("git commit failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// git refuses an empty name (an empty email is fine), but a source commit
/// can carry one; fall back to the email so the export still lands.
fn ident_name(p: &Person) -> &str {
    let name = p.name.trim();
    if !name.is_empty() {
        &p.name
    } else if !p.email.trim().is_empty() {
        &p.email
    } else {
        "unknown"
    }
}

/// `GIT_*_DATE` in git's raw form: `@<unix-seconds> <±HHMM>`. The `@` keeps a
/// small timestamp from being misread as another date format. A malformed zone
/// is replaced by `+0000` — git would otherwise silently substitute the LOCAL
/// zone — and a moment git cannot represent (before the epoch once the zone
/// is applied) is clamped to it.
fn git_date(p: &Person) -> String {
    match tz_offset_secs(&p.tz) {
        Some(offset) => git_date_at(p.when, &p.tz, offset),
        None => {
            eprintln!("warning: origin timezone `{}` is not ±HHMM; exporting it as +0000", p.tz);
            git_date_at(p.when, "+0000", 0)
        }
    }
}

/// Seconds east of UTC for a `±HHMM` zone; `None` for anything else.
fn tz_offset_secs(tz: &str) -> Option<i64> {
    let b = tz.as_bytes();
    if b.len() != 5 || !(b[0] == b'+' || b[0] == b'-') || !b[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let (h, m) = (tz[1..3].parse::<i64>().ok()?, tz[3..5].parse::<i64>().ok()?);
    if h >= 24 || m >= 60 {
        return None;
    }
    let mag = h * 3600 + m * 60;
    Some(if b[0] == b'-' { -mag } else { mag })
}

fn git_date_at(when: i64, tz: &str, offset_secs: i64) -> String {
    if when + offset_secs < 0 {
        eprintln!("warning: origin date {when} {tz} is before the Unix epoch; exporting it as the epoch");
        return "@0 +0000".to_string();
    }
    format!("@{when} {tz}")
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() { "(empty prompt)".to_string() } else { line.to_string() }
}

fn kind_summary(kind: &lex_vcs::OperationKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.get("op").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_else(|| "op".to_string())
}

/// Replay a transition into the running head map. Mirrors
/// `lex_store`'s own private `apply_transition`; kept in sync with the
/// `StageTransition` variants (the enum is `#[non_exhaustive]`-free, so
/// the compiler flags a new variant here).
fn apply_transition(map: &mut BTreeMap<String, String>, t: &StageTransition) {
    match t {
        StageTransition::Create { sig_id, stage_id }
        | StageTransition::Replace { sig_id, to: stage_id, .. } => {
            map.insert(sig_id.clone(), stage_id.clone());
        }
        StageTransition::Remove { sig_id, .. } => {
            map.remove(sig_id);
        }
        StageTransition::Rename { from, to, body_stage_id } => {
            map.remove(from);
            map.insert(to.clone(), body_stage_id.clone());
        }
        // FilesOnly (#1007): the files manifest is rendered separately
        // (PR 6); the sig->stage map is untouched.
        StageTransition::ImportOnly | StageTransition::FilesOnly => {}
        StageTransition::Merge { entries } => {
            for (sig, stage) in entries {
                match stage {
                    Some(s) => { map.insert(sig.clone(), s.clone()); }
                    None => { map.remove(sig); }
                }
            }
        }
    }
}

/// Write one manifest entry's blob to `out_dir/path`, restoring its
/// executable bit (`Entry::mode`). Creates parent directories as needed.
fn write_manifest_entry(store: &Store, out_dir: &Path, path: &str, entry: &FileEntry) -> Result<()> {
    let bytes = store.get_blob_bytes(&entry.blob).map_err(|e| anyhow!("{e}"))?;
    let full = out_dir.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&full, &bytes).with_context(|| format!("writing {}", full.display()))?;
    #[cfg(unix)]
    if entry.mode == MODE_EXEC {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&full)?.permissions();
        perm.set_mode(perm.mode() | 0o111);
        std::fs::set_permissions(&full, perm)?;
    }
    Ok(())
}

/// Remove `out_dir/path` if present. Missing is not an error — the
/// preceding `src/` wipe may already have taken a src/-nested path out.
fn remove_manifest_path(out_dir: &Path, path: &str) -> Result<()> {
    let full = out_dir.join(path);
    match std::fs::remove_file(&full) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", full.display())),
    }
}

/// Apply the on-disk delta from `old` to `new`: drop a path the new
/// manifest no longer carries, (re)write a path that's new or whose entry
/// (blob or mode) changed. A path under `src/` is skipped here — the
/// caller already re-materialized every such path, unconditionally, right
/// after the render step wiped `src/` (see the call site) — so the only
/// thing left for a src/-nested path is a removal, which the first loop
/// below still covers (a no-op on disk, since the wipe got there first,
/// but it keeps `git add -A` honest about the manifest's own bookkeeping).
fn apply_manifest_diff(store: &Store, out_dir: &Path, old: &Manifest, new: &Manifest) -> Result<()> {
    for path in old.entries.keys() {
        if !new.entries.contains_key(path) {
            remove_manifest_path(out_dir, path)?;
        }
    }
    for (path, entry) in &new.entries {
        if path.starts_with("src/") {
            continue;
        }
        if old.entries.get(path) != Some(entry) {
            write_manifest_entry(store, out_dir, path, entry)?;
        }
    }
    Ok(())
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
