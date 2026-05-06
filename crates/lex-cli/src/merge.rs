//! `lex merge` — CLI mirror of `POST /v1/merge/{start,<id>/resolve,
//! <id>/commit}` (#134). Runs the same MergeSession engine
//! directly (no HTTP server needed) and persists in-flight
//! sessions to `<store>/merges/<merge_id>.json` so each
//! invocation is its own process.
//!
//! Subcommands:
//!
//!   lex merge start  --src <B1> --dst <B2> [--store DIR]
//!   lex merge status <merge_id>            [--store DIR]
//!   lex merge resolve <merge_id> --file <resolutions.json> [--store DIR]
//!   lex merge commit <merge_id>            [--store DIR]
//!
//! The resolutions file is a JSON array of
//! `{"conflict_id": "...", "resolution": {"kind": "take_ours" | ... }}`.
//! Same shape as the HTTP /resolve body's `resolutions` field.

use crate::acli as acli_mod;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On-disk envelope for an in-flight merge. Wraps the lex-vcs
/// `MergeSession` with the branch labels the start command captured
/// (the engine session itself only carries OpId heads, not branch
/// names; commit needs the dst branch to advance the right head).
#[derive(Debug, Serialize, Deserialize)]
struct MergeFile {
    src_branch: String,
    dst_branch: String,
    session: lex_vcs::MergeSession,
}

fn merges_dir(root: &Path) -> PathBuf {
    root.join("merges")
}

fn merge_path(root: &Path, merge_id: &str) -> PathBuf {
    merges_dir(root).join(format!("{merge_id}.json"))
}

fn load_merge(root: &Path, merge_id: &str) -> Result<MergeFile> {
    let p = merge_path(root, merge_id);
    let bytes = std::fs::read(&p)
        .with_context(|| format!("read merge session {}", p.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse merge session {}", p.display()))
}

fn save_merge(root: &Path, file: &MergeFile) -> Result<()> {
    std::fs::create_dir_all(merges_dir(root))?;
    let p = merge_path(root, &file.session.merge_id);
    let bytes = serde_json::to_vec_pretty(file)?;
    std::fs::write(&p, bytes)
        .with_context(|| format!("write merge session {}", p.display()))
}

fn delete_merge(root: &Path, merge_id: &str) -> Result<()> {
    let p = merge_path(root, merge_id);
    if p.exists() {
        std::fs::remove_file(&p)
            .with_context(|| format!("remove merge session {}", p.display()))?;
    }
    Ok(())
}

fn parse_store_arg(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root: Option<PathBuf> = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--store" {
            if let Some(v) = args.get(i + 1) {
                root = Some(PathBuf::from(v));
                i += 2;
                continue;
            }
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (root.unwrap_or_else(crate::default_store_root_pub), rest)
}

pub fn cmd_merge(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(||
        anyhow!("usage: lex merge {{start|status|resolve|defer|commit}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "start"   => cmd_start(fmt, rest),
        "status"  => cmd_status(fmt, rest),
        "resolve" => cmd_resolve(fmt, rest),
        "defer"   => cmd_defer(fmt, rest),
        "commit"  => cmd_commit(fmt, rest),
        other => bail!("unknown `lex merge` subcommand: {other}"),
    }
}

fn cmd_start(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store_arg(args);
    let mut src: Option<String> = None;
    let mut dst: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--src" => { src = rest.get(i + 1).cloned(); i += 2; }
            "--dst" => { dst = rest.get(i + 1).cloned(); i += 2; }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let src_branch = src.ok_or_else(|| anyhow!("--src <branch> required"))?;
    let dst_branch = dst.ok_or_else(|| anyhow!("--dst <branch> required"))?;
    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;

    let src_head = store.get_branch(&src_branch)?
        .ok_or_else(|| anyhow!("unknown src branch `{src_branch}`"))?
        .head_op;
    let dst_head = store.get_branch(&dst_branch)?
        .ok_or_else(|| anyhow!("unknown dst branch `{dst_branch}`"))?
        .head_op;
    let log = lex_vcs::OpLog::open(store.root())?;
    let merge_id = mint_merge_id();
    let session = lex_vcs::MergeSession::start(
        merge_id.clone(),
        &log,
        src_head.as_ref(),
        dst_head.as_ref(),
    )?;
    let conflicts: Vec<lex_vcs::ConflictRecord> = session
        .remaining_conflicts()
        .into_iter()
        .cloned()
        .collect();
    let auto_resolved_count = session.auto_resolved.len();

    let file = MergeFile { src_branch, dst_branch, session };
    save_merge(&root, &file)?;

    let data = serde_json::json!({
        "merge_id":            file.session.merge_id,
        "src_head":            file.session.src_head,
        "dst_head":            file.session.dst_head,
        "lca":                 file.session.lca,
        "conflicts":           conflicts,
        "auto_resolved_count": auto_resolved_count,
    });
    let conflicts_for_text = conflicts.clone();
    let merge_id_for_text = file.session.merge_id.clone();
    acli_mod::emit_or_text("merge", data, fmt, move || {
        println!("merge_id: {merge_id_for_text}");
        println!("conflicts: {} (auto_resolved: {auto_resolved_count})", conflicts_for_text.len());
        for c in &conflicts_for_text {
            println!("  {} ({:?})", c.conflict_id, c.kind);
        }
    });
    Ok(())
}

fn cmd_status(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store_arg(args);
    let merge_id = rest.first().ok_or_else(|| anyhow!("usage: lex merge status <merge_id>"))?;
    let file = load_merge(&root, merge_id)?;
    let remaining: Vec<lex_vcs::ConflictRecord> = file.session
        .remaining_conflicts()
        .into_iter()
        .cloned()
        .collect();
    let data = serde_json::json!({
        "merge_id":            file.session.merge_id,
        "src_branch":          file.src_branch,
        "dst_branch":          file.dst_branch,
        "remaining_conflicts": remaining,
    });
    let remaining_for_text = remaining.clone();
    let merge_id_for_text  = file.session.merge_id.clone();
    let src_for_text       = file.src_branch.clone();
    let dst_for_text       = file.dst_branch.clone();
    acli_mod::emit_or_text("merge", data, fmt, move || {
        println!("merge_id: {merge_id_for_text}");
        println!("merging:  {src_for_text} → {dst_for_text}");
        println!("remaining: {}", remaining_for_text.len());
        for c in &remaining_for_text {
            println!("  {} ({:?})", c.conflict_id, c.kind);
        }
    });
    Ok(())
}

#[derive(Deserialize)]
struct ResolutionEntry {
    conflict_id: String,
    resolution: lex_vcs::Resolution,
}

fn cmd_resolve(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store_arg(args);
    let mut merge_id: Option<String> = None;
    let mut file_arg: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--file" => { file_arg = rest.get(i + 1).cloned(); i += 2; }
            other if merge_id.is_none() => { merge_id = Some(other.to_string()); i += 1; }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let merge_id  = merge_id.ok_or_else(|| anyhow!("usage: lex merge resolve <merge_id> --file <resolutions.json>"))?;
    let file_path = file_arg.ok_or_else(|| anyhow!("--file <resolutions.json> required"))?;
    let raw = std::fs::read(&file_path)
        .with_context(|| format!("read resolutions file {file_path}"))?;
    let entries: Vec<ResolutionEntry> = serde_json::from_slice(&raw)
        .with_context(|| format!("parse resolutions file {file_path}"))?;

    let mut file = load_merge(&root, &merge_id)?;
    let pairs: Vec<(String, lex_vcs::Resolution)> = entries.into_iter()
        .map(|e| (e.conflict_id, e.resolution))
        .collect();
    let verdicts = file.session.resolve(pairs);
    let remaining: Vec<lex_vcs::ConflictRecord> = file.session
        .remaining_conflicts()
        .into_iter()
        .cloned()
        .collect();
    save_merge(&root, &file)?;

    let data = serde_json::json!({
        "verdicts":            verdicts,
        "remaining_conflicts": remaining,
    });
    let verdicts_for_text = verdicts.clone();
    let remaining_for_text = remaining.clone();
    acli_mod::emit_or_text("merge", data, fmt, move || {
        for v in &verdicts_for_text {
            let mark = if v.accepted { "✓" } else { "✗" };
            println!("  {mark} {}", v.conflict_id);
        }
        println!("remaining: {}", remaining_for_text.len());
    });
    Ok(())
}

/// `lex merge defer <merge_id> <conflict_id> [--store DIR]` —
/// per-conflict shortcut that submits `Resolution::Defer` against
/// a single conflict (#181). Equivalent to feeding `lex merge
/// resolve` a one-line JSON file but ergonomic for the common
/// "I don't have an opinion on this conflict, leave it for
/// review" workflow that would otherwise require a tempfile.
fn cmd_defer(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store_arg(args);
    let mut positional: Vec<String> = Vec::with_capacity(2);
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            other if !other.starts_with("--") && positional.len() < 2 => {
                positional.push(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let merge_id = positional.first().ok_or_else(||
        anyhow!("usage: lex merge defer <merge_id> <conflict_id>"))?.clone();
    let conflict_id = positional.get(1).ok_or_else(||
        anyhow!("usage: lex merge defer <merge_id> <conflict_id>"))?.clone();

    let mut file = load_merge(&root, &merge_id)?;
    let pairs = vec![(conflict_id.clone(), lex_vcs::Resolution::Defer)];
    let verdicts = file.session.resolve(pairs);
    let remaining: Vec<lex_vcs::ConflictRecord> = file.session
        .remaining_conflicts()
        .into_iter()
        .cloned()
        .collect();
    save_merge(&root, &file)?;

    let data = serde_json::json!({
        "verdicts":            verdicts,
        "remaining_conflicts": remaining,
    });
    let conflict_id_for_text = conflict_id.clone();
    let verdict_for_text = verdicts.first().cloned();
    acli_mod::emit_or_text("merge", data, fmt, move || {
        match verdict_for_text {
            Some(v) if v.accepted => println!("✓ deferred {conflict_id_for_text}"),
            Some(v) => {
                let why = v.rejection
                    .map(|r| format!("{r:?}"))
                    .unwrap_or_else(|| "rejected".into());
                println!("✗ {conflict_id_for_text}: {why}");
            }
            None => println!("(no verdict returned)"),
        }
    });
    Ok(())
}

fn cmd_commit(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store_arg(args);
    let merge_id = rest.first().ok_or_else(|| anyhow!("usage: lex merge commit <merge_id>"))?;
    let file = load_merge(&root, merge_id)?;
    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;

    let dst_branch = file.dst_branch.clone();
    let src_head   = file.session.src_head.clone();
    let dst_head   = file.session.dst_head.clone();
    let auto_resolved = file.session.auto_resolved.clone();

    // Translate auto-resolved + resolutions into the
    // StageTransition::Merge entries map. Mirrors the HTTP
    // /v1/merge/<id>/commit handler.
    let mut entries: BTreeMap<lex_vcs::SigId, Option<lex_vcs::StageId>> = BTreeMap::new();
    for outcome in &auto_resolved {
        if let lex_vcs::MergeOutcome::Src { sig_id, stage_id } = outcome {
            entries.insert(sig_id.clone(), stage_id.clone());
        }
    }

    let resolved = match file.session.commit() {
        Ok(r) => r,
        Err(lex_vcs::CommitError::ConflictsRemaining(ids)) => {
            bail!("conflicts remaining: {}", ids.join(", "));
        }
    };

    for (conflict_id, resolution) in resolved {
        match resolution {
            lex_vcs::Resolution::TakeOurs => {}
            lex_vcs::Resolution::TakeTheirs => {
                let stage_id = walk_src_for_sig(&store, &src_head, &conflict_id)?;
                entries.insert(conflict_id, stage_id);
            }
            lex_vcs::Resolution::Custom { op } => {
                let (sig, stage) = op.kind.merge_target()
                    .ok_or_else(|| anyhow!(
                        "custom op kind doesn't yield a single sig→stage delta: {:?}", op.kind))?;
                if sig != conflict_id {
                    bail!("custom op targets sig `{sig}` but the conflict is on `{conflict_id}`");
                }
                entries.insert(conflict_id, stage);
            }
            lex_vcs::Resolution::Defer => {
                bail!("internal: Defer resolution slipped past commit gate");
            }
        }
    }

    let resolved_count = entries.len();
    let mut parents: Vec<lex_vcs::OpId> = Vec::new();
    if let Some(d) = dst_head { parents.push(d); }
    if let Some(s) = src_head { parents.push(s); }
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::Merge { resolved: resolved_count },
        parents,
    );
    let transition = lex_vcs::StageTransition::Merge { entries };
    let new_head_op = store.apply_operation(&dst_branch, op, transition)?;

    delete_merge(&root, merge_id)?;

    let data = serde_json::json!({
        "new_head_op": new_head_op,
        "dst_branch":  dst_branch,
    });
    let new_head_for_text = new_head_op.clone();
    let dst_for_text = dst_branch.clone();
    acli_mod::emit_or_text("merge", data, fmt, move || {
        println!("merged {dst_for_text}: head_op = {new_head_for_text}");
    });
    Ok(())
}

fn walk_src_for_sig(
    store: &Store,
    src_head: &Option<lex_vcs::OpId>,
    sig: &lex_vcs::SigId,
) -> Result<Option<lex_vcs::StageId>> {
    let log = lex_vcs::OpLog::open(store.root())?;
    let Some(head) = src_head.as_ref() else { return Ok(None); };
    let mut current: Option<lex_vcs::StageId> = None;
    for record in log.walk_forward(head, None)? {
        match &record.produces {
            lex_vcs::StageTransition::Create { sig_id, stage_id }
                if sig_id == sig => { current = Some(stage_id.clone()); }
            lex_vcs::StageTransition::Replace { sig_id, to, .. }
                if sig_id == sig => { current = Some(to.clone()); }
            lex_vcs::StageTransition::Remove { sig_id, .. }
                if sig_id == sig => { current = None; }
            lex_vcs::StageTransition::Rename { from, to, body_stage_id }
                if from == sig || to == sig => {
                if from == sig { current = None; }
                if to == sig   { current = Some(body_stage_id.clone()); }
            }
            lex_vcs::StageTransition::Merge { entries } => {
                if let Some(opt) = entries.get(sig) {
                    current = opt.clone();
                }
            }
            _ => {}
        }
    }
    Ok(current)
}

fn mint_merge_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("merge_{nanos:x}_{n:x}")
}
