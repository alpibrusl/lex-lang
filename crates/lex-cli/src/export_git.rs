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
//! typed ops via `diff_to_ops`) and per-op author/date from the intent's
//! model are follow-ups on #837.
//!
//! Usage:
//!   lex export-git <out_dir> [--branch NAME] [--store DIR]

use super::*;
use lex_vcs::{default_import_alias, IntentLog, OpLog, OperationKind, StageTransition};
use std::collections::BTreeMap;
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
    let mut flat_imports: BTreeMap<String, String> = BTreeMap::new();
    let mut file_imports: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    // SigId → the source file its declaration came from (from each
    // AddFunction/AddType's `in_file`). When every head stage has one, the
    // package was published multi-module and we de-flatten it back into a
    // `src/*.lex` tree; otherwise we render one `src.lex`.
    let mut sig_files: BTreeMap<String, String> = BTreeMap::new();
    let mut commits = 0usize;

    for rec in &records {
        apply_transition(&mut map, &rec.produces);
        match &rec.op.kind {
            OperationKind::AddFunction { sig_id, in_file: Some(f), .. }
            | OperationKind::AddType { sig_id, in_file: Some(f), .. } => {
                sig_files.insert(sig_id.clone(), f.clone());
            }
            OperationKind::AddImport { in_file, module, alias } => {
                // The op omits the alias when it's the module's default;
                // reconstruct it the same way the store does.
                let alias = alias.clone().unwrap_or_else(|| default_import_alias(module));
                flat_imports.insert(module.clone(), alias.clone());
                file_imports.entry(in_file.clone()).or_default().insert(module.clone(), alias);
            }
            OperationKind::RemoveImport { in_file, module } => {
                flat_imports.remove(module);
                if let Some(m) = file_imports.get_mut(in_file) {
                    m.remove(module);
                }
            }
            OperationKind::RenameSymbol { from, to, .. } => {
                if let Some(f) = sig_files.remove(from) {
                    sig_files.insert(to.clone(), f);
                }
            }
            _ => {}
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
            flat_imports: flat_imports.clone(),
            file_imports: file_imports.clone(),
        };
        match lex_store::render::render_source(&store, &head)? {
            lex_store::render::RenderedSource::Single(src) => {
                std::fs::write(&src_path, src)
                    .with_context(|| format!("writing {}", src_path.display()))?;
            }
            lex_store::render::RenderedSource::Multi(tree) => {
                for (relpath, src) in tree {
                    let path = out_dir.join(&relpath);
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&path, src)
                        .with_context(|| format!("writing {}", path.display()))?;
                }
            }
        }

        // Commit message: the intent prompt, else a kind summary.
        let msg = commit_message(&intents, rec)?;
        run_git(&out_dir, &["add", "-A"])?;
        // --allow-empty: an ImportOnly op (or a no-op transition)
        // doesn't change the tree, but the commit still records the op.
        run_git(&out_dir, &["commit", "-q", "--allow-empty", "-m", &msg])?;
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
/// a trailer so the git view is traceable back to the log.
fn commit_message(intents: &IntentLog, rec: &lex_vcs::OperationRecord) -> Result<String> {
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
    Ok(format!("{subject}\n\nOp: {}{intent_line}", rec.op_id))
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
        StageTransition::ImportOnly => {}
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
