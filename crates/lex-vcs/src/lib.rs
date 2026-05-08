//! Agent-native version control for Lex (#128 tier-2).
//!
//! The unit of writing is an [`Operation`] — a typed delta on the AST
//! identified by `(kind, payload, parents)`. Two agents producing the
//! same logical change against the same parent state get the same
//! [`OpId`], so the store can dedup automatically and surface "we
//! agree" without a merge.
//!
//! This crate is the foundation slice of #129. It defines the
//! operation enum and content-addressed identity. Subsequent slices
//! add: applying ops to a store state (#129 cont'd), the write-time
//! type-check gate (#130), intent linkage (#131), attestations
//! (#132), predicate branches (#133), and the programmatic merge API
//! (#134).
//!
//! # Identity
//!
//! [`OpId`] is the lowercase-hex SHA-256 of the canonical JSON form
//! of `(kind, payload, parents)`. The serializer is deterministic
//! by construction (struct fields are emitted in declaration order;
//! [`EffectSet`] is a `BTreeSet`; parents are sorted before
//! hashing), so two independent runs producing the same logical
//! operation produce byte-identical canonical bytes.
//!
//! `lex-store` already uses SHA-256 (via the `sha2` crate) for stage
//! and signature identity, so we reuse that here for consistency
//! and to avoid pulling in a second hash dependency. The issue text
//! mentions Blake3; if that becomes load-bearing for performance we
//! can swap with a one-line crate change since `OpId` is opaque.

mod apply;
mod attestation;
mod canonical;
mod compute_diff;
pub mod diff_report;
mod diff_to_ops;
mod gate;
mod intent;
mod merge;
mod merge_session;
pub mod migrate;
mod op_log;
mod operation;
mod predicate;
pub mod signing;

pub use apply::{apply, ApplyError, NewHead};
pub use attestation::{
    is_stage_blocked, Attestation, AttestationId, AttestationKind, AttestationLog,
    AttestationResult, ContentHash, Cost, ProducerDescriptor, Signature, SpecId, SpecMethod,
};
pub use compute_diff::{compute_diff, effect_label, render_signature};
pub use diff_report::DiffReport;
pub use diff_to_ops::{diff_to_ops, DiffInputs, DiffMappingError, ImportMap};
pub use gate::{check_and_apply, GateError};
pub use intent::{Intent, IntentId, IntentLog, ModelDescriptor, SessionId};
pub use merge::{merge, ConflictKind, MergeOutcome, MergeOutput};
pub use merge_session::{
    CommitError, ConflictId, ConflictRecord, MergeSession, MergeSessionId, ResolveVerdict,
    Resolution, ResolutionRejection,
};
pub use predicate::{evaluate, evaluate_with_resolver, IntentResolver, Predicate};
pub use signing::{verify_stage_id, Keypair, SigningError};
pub use op_log::OpLog;
pub use operation::{
    EffectSet, ModuleRef, OpId, Operation, OperationFormat, OperationKind, OperationRecord, SigId,
    StageId, StageTransition,
};
