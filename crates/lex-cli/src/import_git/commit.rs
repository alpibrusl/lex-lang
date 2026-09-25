//! One commit → ops: the per-commit half of the importer.
//!
//! [`import_commit`] is what the history loop calls once per commit. It brings
//! the incremental tree to the commit ([`ImportState::advance_tree`]), decides
//! whether the semantic pass must run (only when the commit touched
//! `lex.toml`/`lex.lock`/`src/**/*.lex`, or a previous refusal left the store
//! behind the tree), applies the ops on the private work branch, and either
//! keeps them or puts the branch back exactly as it was.

use super::git::{read_commit, CommitMeta};
use super::tree::Class;
use super::{publish_core, CommitOutcome, ImportState, Refusal, IMPORT_PROFILE_VERSION};
use crate::publish_core::{Outcome, PublishError, PublishOptions};
#[cfg(test)]
use anyhow::bail;
use anyhow::{anyhow, Context, Result};
use lex_store::files::{Entry, Manifest, MODE_EXEC, MODE_FILE};
use lex_vcs::{Intent, IntentLog, ModelDescriptor, Origin};

/// The deterministic intent of `meta` (see the module docs of `import_git`).
pub(super) fn build_intent(root_sha: &str, meta: &CommitMeta, folded: &[String]) -> Intent {
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

/// Whether the current tree is a loadable Lex package, or why it is not
/// importable. `Ok(false)` is a tree with no Lex sources at all (a non-Lex
/// repo, or a commit that deleted the package).
fn classify_package(state: &ImportState) -> std::result::Result<bool, Refusal> {
    let t = &state.tree;
    if t.lex_src_count == 0 && !t.root_src_lex {
        return Ok(false);
    }
    // `src/**/*.lex` (or `src.lex`) present: those paths are reserved (they
    // cannot go in a manifest), so the tree MUST be a loadable package.
    if t.root_src_lex {
        return Err(Refusal::new(
            "lex:root_src_lex",
            "tree",
            "`src.lex` at the repository root is op-log-owned and not importable; \
             a Lex package keeps its sources under src/",
        ));
    }
    if !t.files.contains_key("lex.toml") {
        return Err(Refusal::new(
            "lex:no_manifest",
            "tree",
            "the tree has src/**/*.lex but no lex.toml: those paths are reserved for the \
             op-log and cannot be carried in a files manifest, so this is not importable",
        ));
    }
    if !t.toml_has_name {
        return Err(Refusal::new(
            "lex:not_a_package",
            "tree",
            "the tree has src/**/*.lex but its lex.toml has no [package] name",
        ));
    }
    Ok(true)
}

/// Import ONE commit onto `state.work_branch`, atomically: semantic ops first,
/// `SetFiles` last, one intent — or nothing at all. The history loop calls
/// this once per commit, in first-parent order.
///
/// `Ok(Refused(..))` is an expected outcome (a gate said no); `Err` is an
/// infrastructure failure (git, IO). Either way the work branch is restored.
pub(super) fn import_commit(state: &mut ImportState, sha: &str) -> Result<CommitOutcome> {
    let meta = read_commit(&state.repo, sha)?;
    state.stats.commits += 1;
    state.last_parents = meta.parents.clone();
    let advance = match state.advance_tree(sha) {
        Ok(a) => a,
        Err(e) => {
            state.reset_tree();
            return Err(e);
        }
    };
    let refusal = if state.strict && !state.tree.unsupported_now.is_empty() {
        Some(Refusal::new(
            "strict:unsupported",
            "strict",
            format!(
                "--strict: {} unsupported path(s) (symlinks/submodules) in the tree",
                state.tree.unsupported_now.len()
            ),
        ))
    } else {
        state.tree.blocker()
    };
    let refusal = match refusal {
        Some(r) => Some(r),
        None => classify_package(state).err(),
    };
    if let Some(r) = refusal {
        // The tree moved on but the store did not.
        state.dirty = true;
        return Ok(CommitOutcome::Refused(r));
    }
    let is_package = classify_package(state).expect("checked above");

    let intent = build_intent(&state.root_sha, &meta, &state.pending_folded);
    let work = state.work_branch.clone();
    let before = state.store.get_branch(&work)?.and_then(|b| b.head_op);
    let ckpt = format!("{work}.ckpt");
    state.store.create_branch(&ckpt, &work).map_err(|e| anyhow!("{e}"))?;

    let result = apply_commit(state, is_package, advance.semantic_touched, &intent);

    match &result {
        Ok(CommitOutcome::Imported { .. }) => {
            super::delete_branch(&state.store_root, &state.store, &ckpt)?;
            state.pending_folded.clear();
        }
        _ => {
            // Refused, Noop or an infrastructure error. If ops landed on the
            // work branch (a semantic pass that succeeded before a later step
            // failed) put it back exactly as it was; if the head did not move
            // there is nothing to undo.
            let after = state.store.get_branch(&work)?.and_then(|b| b.head_op);
            if after != before {
                super::delete_branch(&state.store_root, &state.store, &work)?;
                state.store.create_branch(&work, &ckpt).map_err(|e| anyhow!("{e}"))?;
            }
            super::delete_branch(&state.store_root, &state.store, &ckpt)?;
            if !matches!(result, Ok(CommitOutcome::Noop)) {
                // The tree moved on but the store did not: the next commit
                // must re-run the semantic pass so it catches up.
                state.dirty = true;
            }
        }
    }
    result
}

/// The files manifest of the current tree, built from the in-memory blob ids
/// (no object is read).
fn manifest_of(state: &ImportState) -> Manifest {
    let mut m = Manifest::new();
    for (path, f) in &state.tree.files {
        if f.class != Class::Manifest {
            continue;
        }
        let blob = f.blob.clone().expect("a manifest file has a stored blob");
        m.entries.insert(
            path.clone(),
            Entry { blob, mode: (if f.exec { MODE_EXEC } else { MODE_FILE }).to_string(), size: f.size },
        );
    }
    m
}

/// The mutating half of [`import_commit`]; the caller owns the checkpoint.
fn apply_commit(
    state: &mut ImportState,
    is_package: bool,
    touched: bool,
    intent: &Intent,
) -> Result<CommitOutcome> {
    let work = state.work_branch.clone();
    let mut ops = 0usize;

    // ── the semantic pass ───────────────────────────────────────────────────
    // Needed when the commit touched a semantic path, or when an earlier
    // refusal left the store behind the tree (folding). A commit that touches
    // none of them is a `SetFiles`-only commit and skips the (expensive)
    // load + type-check entirely.
    //
    // A tree with no Lex sources but a store with live declarations is a
    // commit that DELETED the package: the pass runs on the scratch dir with
    // no `.lex` files (the empty program, `allow_empty`), so `diff_to_ops`
    // emits the removals.
    let run = touched || state.dirty;
    let semantic = if !run {
        false
    } else if is_package {
        true
    } else {
        !state.store.branch_head(&work)?.is_empty()
    };
    if semantic {
        state.stats.semantic_passes += 1;
        let mut opts = PublishOptions::new(intent.clone());
        opts.files = false; // the manifest is built from git objects, below
        opts.examples = state.examples;
        opts.allow_empty = true;
        match publish_core::publish_dir(&state.store_root, state.scratch.path(), Some(&work), opts) {
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
    } else {
        state.stats.semantic_skipped += 1;
    }
    #[cfg(test)]
    if state.fail_after_semantic {
        state.saw_partial_head = state.store.get_branch(&work)?.and_then(|b| b.head_op).is_some();
        bail!("injected failure after the semantic ops");
    }

    // ── the manifest, LAST, under the same intent ──────────────────────────
    let manifest = manifest_of(state);
    if let Err(e) = manifest.validate() {
        return Ok(CommitOutcome::Refused(Refusal::new(
            "manifest:invalid",
            "manifest",
            format!("building files manifest: {e}"),
        )));
    }
    let current = state.store.branch_manifest(&work)?;
    // An empty manifest on a branch that never had one is "no files", not a
    // `SetFiles` op (an empty root commit must stay a no-op).
    let unchanged = match current.manifest() {
        Some(id) => *id == manifest.id(),
        None => manifest.entries.is_empty(),
    };
    let mut files_op = None;
    if !unchanged {
        IntentLog::open(state.store.root())
            .and_then(|l| l.put(intent))
            .context("recording intent")?;
        match crate::files::publish_manifest_if_changed(
            &state.store,
            &work,
            &manifest,
            Some(intent.intent_id.clone()),
        ) {
            Ok(r) => files_op = r.map(|(op, _)| op),
            Err(e) => {
                return Ok(CommitOutcome::Refused(Refusal::new("gate:store", "manifest", format!("{e:#}"))));
            }
        }
    }
    if files_op.is_some() {
        ops += 1;
    }
    if semantic {
        // The semantic pass caught the store up with the tree.
        state.dirty = false;
    }

    let head = state.store.get_branch(&work)?.and_then(|b| b.head_op);
    if ops == 0 {
        return Ok(CommitOutcome::Noop);
    }
    // The committed lock rides on the final head, as `lex publish` leaves it.
    if let (Some(h), Some(lock)) = (head.as_deref(), state.tree.lock_text.as_deref()) {
        state.store.set_committed_lock(h, lock)?;
    }
    Ok(CommitOutcome::Imported { ops, files_op, head })
}
