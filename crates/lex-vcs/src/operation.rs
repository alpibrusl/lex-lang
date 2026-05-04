//! The `Operation` enum + `OperationRecord` (operation plus its
//! causal parents and resulting `OpId`).
//!
//! See `lib.rs` for the design context and #129 for the issue.

use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::canonical;

/// Signature identity of a function or type — the part that stays
/// stable across body edits. Wraps the same string identity
/// `lex-store` uses; we keep it as `String` here so this crate has
/// no dependency on `lex-store`'s internals.
pub type SigId = String;

/// Content hash of a single stage (function body, type def, ...).
/// Same string identity as the file under `<root>/stages/<SigId>/
/// implementations/<StageId>.ast.json`.
pub type StageId = String;

/// Identity of an operation. `(kind, payload, parents)` SHA-256 in
/// lowercase hex (64 chars). Two operations with identical payloads
/// and parent sets produce identical `OpId`s; the store dedupes on
/// this.
pub type OpId = String;

/// Sorted set of effect-kind strings (e.g. `["fs_write", "io"]`).
/// `BTreeSet` so the canonical form is order-independent for
/// hashing.
pub type EffectSet = BTreeSet<String>;

/// Reference to an imported module — either a stdlib name
/// (`std.io`) or a local path (`./helpers`). Kept as a string so
/// this crate doesn't pull in `lex-syntax`'s parser.
pub type ModuleRef = String;

/// Effect of applying an operation on a stage's content-addressed
/// identity. Used as the `produces` field of an [`OperationRecord`]
/// so consumers can answer "after this op, what's the head stage
/// for this SigId?" without rerunning the apply step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StageTransition {
    /// New SigId; produces a stage that didn't exist before.
    Create { sig_id: SigId, stage_id: StageId },
    /// Existing SigId; replaces its head stage.
    Replace { sig_id: SigId, from: StageId, to: StageId },
    /// SigId removed; no head stage afterwards.
    Remove { sig_id: SigId, last: StageId },
    /// SigId renamed; same body hash, different signature identity.
    Rename { from: SigId, to: SigId, body_stage_id: StageId },
    /// Import-only change; doesn't touch any stage.
    ImportOnly,
    /// Merge op result. `entries` lists only the sigs whose head
    /// changed relative to the merge op's first parent (`dst_head`):
    /// `Some(stage_id)` sets the head; `None` removes the sig.
    /// Sigs unaffected by the merge are not listed.
    Merge {
        entries: BTreeMap<SigId, Option<StageId>>,
    },
}

/// The kinds of operations that produce stage transitions. Mirrors
/// the initial set in #129; new kinds (`MoveBetweenFiles`,
/// `SplitFunction`, `ExtractType`) can be added later as long as
/// they're appended at the end of this enum or use explicit
/// `#[serde(rename = "...")]` tags so existing `OpId`s stay stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OperationKind {
    /// New function published. `effects` is the effect set declared
    /// in the signature; tracked here (not just inside the stage)
    /// so #130's write-time gate has a cheap path to check effect
    /// changes without rehydrating the AST.
    AddFunction {
        sig_id: SigId,
        stage_id: StageId,
        effects: EffectSet,
    },
    /// Function removed; `last_stage_id` is the head before the
    /// remove (so blame can walk the predecessor without scanning).
    RemoveFunction {
        sig_id: SigId,
        last_stage_id: StageId,
    },
    /// Function body changed; signature unchanged.
    ModifyBody {
        sig_id: SigId,
        from_stage_id: StageId,
        to_stage_id: StageId,
    },
    /// Symbol renamed. The body hash is preserved (`body_stage_id`)
    /// so two renames of the same body collapse to the same OpId
    /// and `lex blame` walks the rename as a single causal event
    /// rather than `delete + add`.
    RenameSymbol {
        from: SigId,
        to: SigId,
        body_stage_id: StageId,
    },
    /// Effect signature changed. Captures both old and new effect
    /// sets so the write-time gate (#130) can verify importers
    /// haven't silently broken.
    ChangeEffectSig {
        sig_id: SigId,
        from_stage_id: StageId,
        to_stage_id: StageId,
        from_effects: EffectSet,
        to_effects: EffectSet,
    },
    /// Import added to a file. `in_file` is the canonical path
    /// (relative to the repo root, forward-slashes) so two
    /// machines hashing the same edit get the same OpId.
    AddImport {
        in_file: String,
        module: ModuleRef,
    },
    RemoveImport {
        in_file: String,
        module: ModuleRef,
    },
    AddType {
        sig_id: SigId,
        stage_id: StageId,
    },
    RemoveType {
        sig_id: SigId,
        last_stage_id: StageId,
    },
    ModifyType {
        sig_id: SigId,
        from_stage_id: StageId,
        to_stage_id: StageId,
    },
    /// Merge of two branch heads. Carries only an informational count
    /// of resolved sigs so two structurally identical merges of
    /// different sizes don't collide on op_id; the per-sig deltas live
    /// in `OperationRecord::produces` (`StageTransition::Merge`).
    Merge {
        resolved: usize,
    },
}

/// The operation as a whole — its kind and the causal predecessors
/// it assumes. The `OpId` is computed from this plus a sorted view
/// of `parents`.
///
/// Operations without parents are valid and represent "applies to
/// the empty repository" or "applies to the synthetic genesis
/// state." `lex store migrate v1→v2` will produce parentless ops
/// for stages it can't trace back to a clear predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    #[serde(flatten)]
    pub kind: OperationKind,
    /// Operations whose `produces` this op assumes. Sorted before
    /// hashing for canonical form. Empty for ops against the empty
    /// repo.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<OpId>,
    /// The intent that caused this op, if known. Optional because
    /// operations produced outside an agent harness (e.g. a human
    /// running `lex publish` directly) don't have one.
    ///
    /// Including the intent in the canonical hash means the same
    /// logical change made under different intents produces
    /// different `OpId`s — causally distinct events should hash
    /// distinctly. Ops with `intent_id: None` keep their existing
    /// hashes (the field is omitted from the canonical JSON via
    /// `skip_serializing_if`), so this is backwards-compatible
    /// for stores written before #131.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<crate::intent::IntentId>,
}

impl Operation {
    /// Construct an operation against zero or more parents. Caller
    /// supplies parents in any order; canonicalization sorts them
    /// before hashing.
    pub fn new(kind: OperationKind, parents: impl IntoIterator<Item = OpId>) -> Self {
        let mut parents: Vec<OpId> = parents.into_iter().collect();
        parents.sort();
        parents.dedup();
        Self { kind, parents, intent_id: None }
    }

    /// Tag this operation with the intent that produced it. The
    /// builder shape keeps existing call sites untouched; agent
    /// harnesses that record intent call this once before
    /// applying the op.
    pub fn with_intent(mut self, intent_id: impl Into<crate::intent::IntentId>) -> Self {
        self.intent_id = Some(intent_id.into());
        self
    }

    /// Compute this operation's content-addressed identity.
    ///
    /// Stable across runs and machines: same `(kind, payload,
    /// sorted parents, intent_id)` produces the same `OpId`. The
    /// invariant #129's automatic-dedup behavior relies on.
    pub fn op_id(&self) -> OpId {
        // Build a transient hashable view rather than hashing
        // `self` directly so the parent ordering is canonical
        // even if a caller hand-constructs an `Operation` with
        // unsorted parents.
        let canonical = CanonicalView {
            kind: &self.kind,
            parents: self.parents.iter().collect::<IndexSet<_>>().into_iter().collect::<BTreeSet<_>>(),
            intent_id: self.intent_id.as_deref(),
        };
        canonical::hash(&canonical)
    }
}

/// Hashable shadow of [`Operation`] with parents in a `BTreeSet` so
/// the serialization is order-independent regardless of how the
/// caller constructed the live operation. Never persisted; lives
/// only as a transient for hashing.
#[derive(Serialize)]
struct CanonicalView<'a> {
    #[serde(flatten)]
    kind: &'a OperationKind,
    parents: BTreeSet<&'a OpId>,
    /// `skip_serializing_if = "Option::is_none"` keeps existing
    /// `OpId`s stable for ops without an intent — the field is
    /// omitted from the canonical JSON entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    intent_id: Option<&'a str>,
}

/// An operation paired with its computed `OpId` and the resulting
/// stage transition. This is what gets persisted under
/// `<root>/ops/<OpId>.json` once `apply.rs` (next slice of #129)
/// lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub op_id: OpId,
    #[serde(flatten)]
    pub op: Operation,
    pub produces: StageTransition,
}

impl OperationRecord {
    pub fn new(op: Operation, produces: StageTransition) -> Self {
        let op_id = op.op_id();
        Self { op_id, op, produces }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_factorial() -> OperationKind {
        OperationKind::AddFunction {
            sig_id: "fac::Int->Int".into(),
            stage_id: "abc123".into(),
            effects: BTreeSet::new(),
        }
    }

    #[test]
    fn identical_operations_have_identical_op_ids() {
        let a = Operation::new(add_factorial(), []);
        let b = Operation::new(add_factorial(), []);
        assert_eq!(a.op_id(), b.op_id());
    }

    #[test]
    fn different_operations_have_different_op_ids() {
        let a = Operation::new(add_factorial(), []);
        let b = Operation::new(
            OperationKind::AddFunction {
                sig_id: "double::Int->Int".into(),
                stage_id: "abc123".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        assert_ne!(a.op_id(), b.op_id());
    }

    #[test]
    fn parent_set_changes_op_id() {
        let no_parent = Operation::new(add_factorial(), []);
        let with_parent = Operation::new(add_factorial(), ["op-parent-1".into()]);
        assert_ne!(no_parent.op_id(), with_parent.op_id());
    }

    #[test]
    fn parent_order_does_not_affect_op_id() {
        let a = Operation::new(add_factorial(), ["b".into(), "a".into(), "c".into()]);
        let b = Operation::new(add_factorial(), ["c".into(), "a".into(), "b".into()]);
        assert_eq!(a.op_id(), b.op_id());
        // and the stored form is sorted.
        assert_eq!(a.parents, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn duplicate_parents_are_deduped() {
        let with_dups = Operation::new(
            add_factorial(),
            ["a".into(), "a".into(), "b".into()],
        );
        let no_dups = Operation::new(
            add_factorial(),
            ["a".into(), "b".into()],
        );
        assert_eq!(with_dups.op_id(), no_dups.op_id());
        assert_eq!(with_dups.parents, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn rename_with_same_body_hashes_equal_across_runs() {
        // Two independent runs producing the same rename against the
        // same parent should produce the same OpId — this is the
        // automatic-dedup property #129 relies on for distributed
        // agents.
        let kind = OperationKind::RenameSymbol {
            from: "parse::Str->Int".into(),
            to: "parse_int::Str->Int".into(),
            body_stage_id: "abc123".into(),
        };
        let a = Operation::new(kind.clone(), ["op-parent".into()]);
        let b = Operation::new(kind, ["op-parent".into()]);
        assert_eq!(a.op_id(), b.op_id());
    }

    #[test]
    fn rename_does_not_collide_with_delete_plus_add() {
        // The whole point of `RenameSymbol` is that it's a different
        // OpId from the (semantically-equivalent) `RemoveFunction +
        // AddFunction` pair. Causal history sees one event, not two.
        let rename = Operation::new(
            OperationKind::RenameSymbol {
                from: "parse::Str->Int".into(),
                to: "parse_int::Str->Int".into(),
                body_stage_id: "abc123".into(),
            },
            ["op-parent".into()],
        );
        let remove = Operation::new(
            OperationKind::RemoveFunction {
                sig_id: "parse::Str->Int".into(),
                last_stage_id: "abc123".into(),
            },
            ["op-parent".into()],
        );
        let add = Operation::new(
            OperationKind::AddFunction {
                sig_id: "parse_int::Str->Int".into(),
                stage_id: "abc123".into(),
                effects: BTreeSet::new(),
            },
            ["op-parent".into()],
        );
        assert_ne!(rename.op_id(), remove.op_id());
        assert_ne!(rename.op_id(), add.op_id());
    }

    #[test]
    fn effect_set_order_does_not_affect_op_id() {
        // Effects are a BTreeSet so iteration is sorted. Build two
        // ops via different insertion orders and confirm the
        // canonical form is identical.
        let a_effects: EffectSet = ["io".into(), "fs_write".into()].into_iter().collect();
        let b_effects: EffectSet = ["fs_write".into(), "io".into()].into_iter().collect();
        let a = Operation::new(
            OperationKind::AddFunction {
                sig_id: "x".into(), stage_id: "s".into(), effects: a_effects,
            },
            [],
        );
        let b = Operation::new(
            OperationKind::AddFunction {
                sig_id: "x".into(), stage_id: "s".into(), effects: b_effects,
            },
            [],
        );
        assert_eq!(a.op_id(), b.op_id());
    }

    #[test]
    fn op_id_is_64_char_lowercase_hex() {
        let id = Operation::new(add_factorial(), []).op_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn round_trip_through_serde_json() {
        let op = Operation::new(
            OperationKind::ChangeEffectSig {
                sig_id: "f".into(),
                from_stage_id: "old".into(),
                to_stage_id: "new".into(),
                from_effects: BTreeSet::new(),
                to_effects: ["io".into()].into_iter().collect(),
            },
            ["op-parent".into()],
        );
        let json = serde_json::to_string(&op).expect("serialize");
        let back: Operation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(op, back);
        assert_eq!(op.op_id(), back.op_id());
    }

    #[test]
    fn operation_record_carries_op_id() {
        let op = Operation::new(add_factorial(), []);
        let expected = op.op_id();
        let rec = OperationRecord::new(
            op,
            StageTransition::Create {
                sig_id: "fac::Int->Int".into(),
                stage_id: "abc123".into(),
            },
        );
        assert_eq!(rec.op_id, expected);
    }

    #[test]
    fn intent_id_is_part_of_op_id_canonical_hash() {
        // The dedup property: same `(kind, parents, intent_id)`
        // produces the same OpId. Different intent_ids on
        // otherwise-identical ops produce different OpIds, so
        // causally distinct events (different prompts) hash
        // distinctly.
        let no_intent = Operation::new(add_factorial(), []);
        let with_intent_a = Operation::new(add_factorial(), [])
            .with_intent("intent-a");
        let with_intent_b = Operation::new(add_factorial(), [])
            .with_intent("intent-b");
        let with_intent_a_again = Operation::new(add_factorial(), [])
            .with_intent("intent-a");

        // No-intent op is distinct from any intent-tagged variant.
        assert_ne!(no_intent.op_id(), with_intent_a.op_id());
        // Different intents → different OpIds.
        assert_ne!(with_intent_a.op_id(), with_intent_b.op_id());
        // Same intent → same OpId (the load-bearing dedup invariant).
        assert_eq!(with_intent_a.op_id(), with_intent_a_again.op_id());
    }

    #[test]
    fn op_without_intent_keeps_pre_intent_op_id() {
        // Backwards-compat invariant: an op constructed without an
        // intent must hash to the same value as it would have
        // before #131 added the field. The golden test below pins
        // the exact hash; this one asserts that adding then
        // resetting to None doesn't drift.
        let mut op = Operation::new(add_factorial(), []);
        let baseline = op.op_id();
        op.intent_id = Some("transient".into());
        let with_intent = op.op_id();
        assert_ne!(baseline, with_intent);
        op.intent_id = None;
        let back = op.op_id();
        assert_eq!(baseline, back);
    }

    /// Golden hash. If this changes, the canonical form has shifted
    /// and *every* op_id in every existing store has changed too —
    /// that's a major-version event for the data model and should
    /// be a deliberate decision, not an accident from reordering
    /// fields. Update with care.
    #[test]
    fn canonical_form_is_stable_for_a_known_input() {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac::Int->Int".into(),
                stage_id: "abc123".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        assert_eq!(
            op.op_id(),
            "f112990d31ef2a63f3e5ca5680637ed36a54bc7e8230510ae0c0e93fcb39d104"
        );
    }

    #[test]
    fn merge_kind_round_trips() {
        let op = Operation::new(
            OperationKind::Merge { resolved: 3 },
            ["op-a".into(), "op-b".into()],
        );
        let json = serde_json::to_string(&op).expect("ser");
        let back: Operation = serde_json::from_str(&json).expect("de");
        assert_eq!(op, back);
        assert_eq!(op.op_id(), back.op_id());
    }

    #[test]
    fn merge_stage_transition_round_trips() {
        let mut entries = BTreeMap::new();
        entries.insert("sig-a".to_string(), Some("stage-a".to_string()));
        entries.insert("sig-b".to_string(), None); // removed by merge
        let t = StageTransition::Merge { entries };
        let json = serde_json::to_string(&t).expect("ser");
        let back: StageTransition = serde_json::from_str(&json).expect("de");
        assert_eq!(t, back);
    }

    #[test]
    fn merge_resolved_count_changes_op_id() {
        // Two merges with the same parents but different resolved counts
        // must hash differently — keeps structurally distinct merges from
        // colliding on op_id.
        let parents: Vec<OpId> = vec!["op-a".into(), "op-b".into()];
        let one = Operation::new(OperationKind::Merge { resolved: 1 }, parents.clone());
        let two = Operation::new(OperationKind::Merge { resolved: 2 }, parents);
        assert_ne!(one.op_id(), two.op_id());
    }

    #[test]
    fn existing_add_function_op_id_is_unchanged_after_merge_added() {
        // Constructing the new Merge variant in the same enum must not
        // perturb the canonical bytes of existing variants. The golden
        // hash test below checks the literal value; this one verifies
        // the property holds even after a Merge op has been built.
        let _merge = Operation::new(
            OperationKind::Merge { resolved: 0 },
            ["op-x".into(), "op-y".into()],
        );
        let op = Operation::new(add_factorial(), []);
        assert_eq!(
            op.op_id(),
            "f112990d31ef2a63f3e5ca5680637ed36a54bc7e8230510ae0c0e93fcb39d104"
        );
    }
}
