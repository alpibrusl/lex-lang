//! Apply an [`Operation`] to a [`Store`], producing the resulting
//! [`OperationRecord`] and a side-effected store state.
//!
//! The apply pass is the bridge from the *typed-delta* world of
//! `lex-vcs` to the *content-addressed-stage* world of `lex-store`.
//! It assumes the caller has already published any new stages
//! (via `Store::publish`) and now wants to advance the store's
//! lifecycle / op log to reflect the change.
//!
//! # Contract
//!
//! - **Stages must exist before the op is applied.** `apply_operation`
//!   does not create stages from source — it only flips lifecycle
//!   states and writes the op record. If the operation references a
//!   `stage_id` that isn't in the store, [`ApplyError::StageMissing`]
//!   is returned and nothing is written.
//! - **Preconditions are checked atomically.** A `ModifyBody` op
//!   whose `from_stage_id` doesn't match the current head returns
//!   [`ApplyError::StaleParent`] without mutating anything.
//! - **The op record is written last.** Any earlier failure
//!   (precondition, lifecycle transition error) leaves no
//!   `<root>/ops/<OpId>.json` file behind.
//!
//! # Persistence layout
//!
//! ```text
//! <root>/
//! ├── ops/
//! │   └── <OpId>.json        ← OperationRecord (this PR)
//! └── stages/...              ← unchanged from lex-store
//! ```
//!
//! Branch heads (`<root>/refs/heads/<branch>`) and the predicate-
//! branch view (#133) are deliberately not touched here. Today's
//! tier-1 branches map `SigId → StageId` and are updated implicitly
//! through `Store::activate` — that path keeps working. The op log
//! is purely additive on top.
//!
//! # What's not here
//!
//! - **`RenameSymbol` apply.** The op enum carries a single
//!   `body_stage_id`, but a rename involves *two* stages (the old
//!   `from`-keyed one and the new `to`-keyed one) because lex-store's
//!   StageId hash includes the function name. Tightening this needs
//!   a small follow-up to the operation enum (separate `from_stage_id`
//!   / `to_stage_id` fields). Until that lands, applying a
//!   `RenameSymbol` returns [`ApplyError::NotYetImplemented`] so
//!   callers don't silently get the wrong store state.
//! - **`diff_to_ops`** — turning an `lex ast-diff` result into a
//!   minimal sequence of operations. Lands with the `lex publish`
//!   refactor.
//! - **The CLI surface** (`lex op show`, `lex op log`, `lex op
//!   apply`). Separate PR.

use std::fs;
use std::path::PathBuf;

use lex_store::{Store, StoreError};

use crate::operation::{
    Operation, OperationKind, OperationRecord, OpId, SigId, StageId, StageTransition,
};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// A required stage isn't in the store. Caller forgot to
    /// `Store::publish` before applying.
    #[error("stage `{0}` not found in store; publish it first")]
    StageMissing(StageId),
    /// `ModifyBody` / `ChangeEffectSig` / `RemoveFunction` etc.
    /// expected a specific head stage and found something else.
    /// Surfaces concurrent-write conflicts as a clean error.
    #[error("stale parent for `{sig_id}`: expected head `{expected}`, found `{actual}`")]
    StaleParent { sig_id: SigId, expected: StageId, actual: String },
    /// `AddFunction` / `AddType` against a `SigId` that already has
    /// an active stage.
    #[error("`{sig_id}` already has an active stage `{existing}`; use ModifyBody to replace")]
    DuplicateAdd { sig_id: SigId, existing: StageId },
    /// Operation kind that the apply pass doesn't yet implement.
    /// Tracked separately so callers can surface a clear "v1.5"
    /// message rather than a generic store error.
    #[error("operation kind not yet implemented in this slice: {0}")]
    NotYetImplemented(&'static str),
}

/// Apply an operation to the store. Returns the resulting
/// [`OperationRecord`] (op id + computed stage transition) and
/// persists it under `<root>/ops/<OpId>.json`.
///
/// See the module docs for the contract: stages must exist;
/// preconditions are checked atomically; the op record is written
/// last so failures leave no footprint.
pub fn apply_operation(store: &Store, op: Operation) -> Result<OperationRecord, ApplyError> {
    let transition = compute_transition(store, &op)?;
    apply_lifecycle(store, &op)?;
    let record = OperationRecord::new(op, transition);
    persist_record(store, &record)?;
    Ok(record)
}

/// Compute the [`StageTransition`] for an op without mutating
/// anything. Pulled out separately so callers (e.g., a future "dry
/// run" mode) can preview the effect.
pub fn compute_transition(
    store: &Store,
    op: &Operation,
) -> Result<StageTransition, ApplyError> {
    match &op.kind {
        OperationKind::AddFunction { sig_id, stage_id, .. }
        | OperationKind::AddType { sig_id, stage_id } => {
            ensure_stage_present(store, stage_id)?;
            ensure_no_active(store, sig_id)?;
            Ok(StageTransition::Create {
                sig_id: sig_id.clone(),
                stage_id: stage_id.clone(),
            })
        }
        OperationKind::RemoveFunction { sig_id, last_stage_id }
        | OperationKind::RemoveType { sig_id, last_stage_id } => {
            ensure_active_matches(store, sig_id, last_stage_id)?;
            Ok(StageTransition::Remove {
                sig_id: sig_id.clone(),
                last: last_stage_id.clone(),
            })
        }
        OperationKind::ModifyBody { sig_id, from_stage_id, to_stage_id }
        | OperationKind::ModifyType { sig_id, from_stage_id, to_stage_id }
        | OperationKind::ChangeEffectSig {
            sig_id, from_stage_id, to_stage_id, ..
        } => {
            ensure_stage_present(store, to_stage_id)?;
            ensure_active_matches(store, sig_id, from_stage_id)?;
            Ok(StageTransition::Replace {
                sig_id: sig_id.clone(),
                from: from_stage_id.clone(),
                to: to_stage_id.clone(),
            })
        }
        OperationKind::AddImport { .. } | OperationKind::RemoveImport { .. } => {
            // Imports don't have a SigId in the store taxonomy
            // today; they live in source files. The op log records
            // them so causal history is complete, but the apply
            // pass has no stage-level work to do.
            Ok(StageTransition::ImportOnly)
        }
        OperationKind::RenameSymbol { from, to, body_stage_id } => {
            // body_stage_id is the FROM stage (lex-store's StageId
            // hash includes the symbol name, so rename produces two
            // stages with different ids). Tightening the op enum to
            // carry both is a small follow-up; until then, surface
            // a clean error rather than silently corrupting state.
            let _ = (from, to, body_stage_id);
            Err(ApplyError::NotYetImplemented("RenameSymbol"))
        }
    }
}

fn apply_lifecycle(store: &Store, op: &Operation) -> Result<(), ApplyError> {
    match &op.kind {
        OperationKind::AddFunction { stage_id, .. }
        | OperationKind::AddType { stage_id, .. } => {
            store.activate(stage_id)?;
        }
        OperationKind::RemoveFunction { last_stage_id, .. }
        | OperationKind::RemoveType { last_stage_id, .. } => {
            // Lex-store enforces the lifecycle ordering Active →
            // Deprecated → Tombstone (a direct Active→Tombstone is
            // rejected). Step through Deprecated first so callers
            // get the same end-state regardless of where they
            // started.
            if store.get_status(last_stage_id)? == lex_store::StageStatus::Active {
                store.deprecate(last_stage_id, "removed")?;
            }
            store.tombstone(last_stage_id)?;
        }
        OperationKind::ModifyBody { to_stage_id, .. }
        | OperationKind::ModifyType { to_stage_id, .. } => {
            // `Store::activate` already demotes the current Active
            // to Deprecated as part of the same lifecycle write —
            // no separate `deprecate` call needed (and a redundant
            // one would error with `Deprecated ⇒ Deprecated`).
            store.activate(to_stage_id)?;
        }
        OperationKind::ChangeEffectSig {
            from_stage_id, to_stage_id, from_effects, to_effects, ..
        } => {
            // Same single-call pattern as ModifyBody. The
            // effect-set diff is captured in the `OperationRecord`
            // for #130's write-time gate; we don't need to
            // re-encode it as a deprecate reason on the old stage.
            let _ = (from_stage_id, from_effects, to_effects);
            store.activate(to_stage_id)?;
        }
        OperationKind::AddImport { .. } | OperationKind::RemoveImport { .. } => {
            // No store-level mutation.
        }
        OperationKind::RenameSymbol { .. } => {
            // Unreachable — `compute_transition` already returned
            // NotYetImplemented and short-circuited.
            return Err(ApplyError::NotYetImplemented("RenameSymbol"));
        }
    }
    Ok(())
}

fn persist_record(store: &Store, record: &OperationRecord) -> Result<(), ApplyError> {
    let dir = ops_dir(store);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", record.op_id));
    let bytes = serde_json::to_vec_pretty(record)?;
    fs::write(path, bytes)?;
    Ok(())
}

/// Read an op record from disk. Returns `Ok(None)` if the file
/// doesn't exist (the more common precondition than a bare error).
pub fn load_record(store: &Store, op_id: &OpId) -> Result<Option<OperationRecord>, ApplyError> {
    let path = ops_dir(store).join(format!("{op_id}.json"));
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn ops_dir(store: &Store) -> PathBuf {
    store.root().join("ops")
}

fn ensure_stage_present(store: &Store, stage_id: &StageId) -> Result<(), ApplyError> {
    match store.get_ast(stage_id) {
        Ok(_) => Ok(()),
        Err(StoreError::UnknownStage(_)) => Err(ApplyError::StageMissing(stage_id.clone())),
        Err(e) => Err(ApplyError::Store(e)),
    }
}

fn ensure_no_active(store: &Store, sig_id: &SigId) -> Result<(), ApplyError> {
    match store.resolve_sig(sig_id) {
        Ok(Some(existing)) => Err(ApplyError::DuplicateAdd {
            sig_id: sig_id.clone(),
            existing,
        }),
        Ok(None) => Ok(()),
        Err(StoreError::UnknownSig(_)) => Ok(()),
        Err(e) => Err(ApplyError::Store(e)),
    }
}

fn ensure_active_matches(
    store: &Store,
    sig_id: &SigId,
    expected: &StageId,
) -> Result<(), ApplyError> {
    match store.resolve_sig(sig_id) {
        Ok(Some(actual)) if &actual == expected => Ok(()),
        Ok(Some(actual)) => Err(ApplyError::StaleParent {
            sig_id: sig_id.clone(),
            expected: expected.clone(),
            actual,
        }),
        Ok(None) => Err(ApplyError::StaleParent {
            sig_id: sig_id.clone(),
            expected: expected.clone(),
            actual: "<no active stage>".into(),
        }),
        Err(StoreError::UnknownSig(_)) => Err(ApplyError::StaleParent {
            sig_id: sig_id.clone(),
            expected: expected.clone(),
            actual: "<unknown sig>".into(),
        }),
        Err(e) => Err(ApplyError::Store(e)),
    }
}

