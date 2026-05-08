//! Predicates over the operation log (#133).
//!
//! Today branches in `lex-store` are pointers: `branch.head_op`
//! resolves to a single [`OpId`], the SigId→StageId map is the
//! transitive ancestry of that op. This is Git's "named pointer to
//! a snapshot" model.
//!
//! It works for human-paced workflows. It breaks for agentic ones:
//! - An agent harness wants to spawn 20 parallel exploration
//!   branches per task and discard 19 of them.
//! - A reviewer wants "show me everything agent X did under intent
//!   Y in the last hour" without that being a pre-named branch.
//! - Two agents working in parallel need to see each other's
//!   pending operations *without* having to merge first.
//!
//! A snapshot-of-pointers model can't answer those questions. A
//! predicate-over-the-log model can.
//!
//! # The predicate
//!
//! A [`Predicate`] is a saved query: "give me the operations
//! matching this filter." Today's `main` branch is
//! `AncestorOf { op_id: <head> }`. A new exploration branch is
//! `Intent { intent_id: <id> }` or
//! `And([Intent { ... }, AncestorOf { op_id: <fork> }])`.
//!
//! Discarding a predicate is `O(1)` — you just stop using it. The
//! operations it referenced stay in the log and stay reachable by
//! other predicates (or by direct `op_id` lookup).
//!
//! # What's deferred
//!
//! - **`Author`**: needs an `author` field on `Operation`. The op
//!   today has `intent_id` (the agent session) but not a separate
//!   "who initiated this" field. Add when the producer chain
//!   surfaces the distinction.
//! - **`DescendantOf`**: needs efficient forward-DAG indexing
//!   (today the log is parent-pointers only). Implementable as a
//!   walk over `OpLog::walk_forward` from the fork point but the
//!   API would be incomplete without an index for the
//!   "performance: 100 branches in a 10k-op store < 1 second" line
//!   in the issue's acceptance criteria. Land separately.
//!
//! # Storage
//!
//! Predicate definitions are JSON files; serialization is
//! tag-rename `serde` so the on-disk form is stable across
//! `lex-vcs` minor versions. A predicate file lives alongside its
//! branch (`<root>/branches/<name>.predicate.json`); reading is
//! lazy (today's branches without a predicate file are treated as
//! `AncestorOf { op_id: head_op }`). Writing branch + predicate
//! files is the consumer's job — this module is the predicate
//! evaluator.

use serde_json::{json, Map, Value};

use crate::intent::{IntentId, SessionId};
use crate::op_log::OpLog;
use crate::operation::{OpId, OperationRecord};

/// A saved query over the operation log. Evaluating against an
/// [`OpLog`] returns the matching [`OperationRecord`]s.
///
/// Serialization is hand-rolled (see the impls below) to avoid the
/// exponential serde-derive monomorphization that recursive enums
/// trigger when other crates in the workspace also derive `Serialize`
/// on rich types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// Every op in the log. The `main` branch's "history" predicate
    /// after migration is `AncestorOf { op_id: <head> }`, not `All`
    /// — `All` is a *different* query ("show me everything in
    /// existence", including ops not yet on any branch).
    All,
    /// Ops produced under a given intent (#131).
    Intent { intent_id: IntentId },
    /// Ops produced from a given agent session (#131).
    /// Matches if any of the intent's session matches; today the op
    /// carries `intent_id`, and the session is a property of the
    /// intent. Resolution is therefore done via the [`crate::IntentLog`]
    /// passed to [`evaluate_with_intents`].
    Session { session_id: SessionId },
    /// Causal ancestry of a given op (the op itself + its parents
    /// transitively). This is what today's named branches map to.
    AncestorOf { op_id: OpId },
    /// All-of: an op matches iff it matches every sub-predicate.
    And(Vec<Predicate>),
    /// Any-of: an op matches iff it matches at least one
    /// sub-predicate.
    Or(Vec<Predicate>),
    /// Negation. Note this requires a corpus to negate over —
    /// `Not(All)` is empty, `Not(AncestorOf { x })` is "every op
    /// not in x's history". Evaluating a top-level `Not` falls
    /// back to scanning the whole log; nesting it under `And` lets
    /// the evaluator narrow the scan to the other clauses' candidate
    /// set first.
    Not(Box<Predicate>),
}

// ---- Serialization (hand-rolled) ---------------------------------
//
// We route through `serde_json::Value`, which has a manual
// `Serialize`/`Deserialize` impl. That keeps the recursive structure
// from triggering the exponential monomorphization that
// `#[derive(Serialize, Deserialize)]` produces on a deeply
// recursive enum.

impl Predicate {
    /// Convert to a `serde_json::Value`. The shape mirrors what an
    /// internally-tagged serde derive would have produced
    /// (`{"predicate": "...", ...}`).
    pub fn to_value(&self) -> Value {
        match self {
            Predicate::All => json!({"predicate": "all"}),
            Predicate::Intent { intent_id } => json!({
                "predicate": "intent",
                "intent_id": intent_id,
            }),
            Predicate::Session { session_id } => json!({
                "predicate": "session",
                "session_id": session_id,
            }),
            Predicate::AncestorOf { op_id } => json!({
                "predicate": "ancestor_of",
                "op_id": op_id,
            }),
            Predicate::And(ps) => {
                let arr: Vec<Value> = ps.iter().map(|p| p.to_value()).collect();
                json!({"predicate": "and", "clauses": arr})
            }
            Predicate::Or(ps) => {
                let arr: Vec<Value> = ps.iter().map(|p| p.to_value()).collect();
                json!({"predicate": "or", "clauses": arr})
            }
            Predicate::Not(p) => json!({
                "predicate": "not",
                "clause": p.to_value(),
            }),
        }
    }

    /// Parse from a `serde_json::Value`. Errors are stringly-typed
    /// because `serde::de::Error` would require pulling in serde
    /// derive paths we're explicitly avoiding.
    pub fn from_value(v: &Value) -> Result<Self, String> {
        let obj: &Map<String, Value> = v
            .as_object()
            .ok_or_else(|| "predicate must be a JSON object".to_string())?;
        let tag = obj
            .get("predicate")
            .and_then(|t| t.as_str())
            .ok_or_else(|| "predicate object missing 'predicate' tag".to_string())?;
        match tag {
            "all" => Ok(Predicate::All),
            "intent" => {
                let id = obj
                    .get("intent_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "intent: missing intent_id".to_string())?
                    .to_string();
                Ok(Predicate::Intent { intent_id: id })
            }
            "session" => {
                let id = obj
                    .get("session_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "session: missing session_id".to_string())?
                    .to_string();
                Ok(Predicate::Session { session_id: id })
            }
            "ancestor_of" => {
                let id = obj
                    .get("op_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| "ancestor_of: missing op_id".to_string())?
                    .to_string();
                Ok(Predicate::AncestorOf { op_id: id })
            }
            "and" | "or" => {
                let arr = obj
                    .get("clauses")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| format!("{tag}: missing 'clauses' array"))?;
                let mut ps = Vec::with_capacity(arr.len());
                for item in arr {
                    ps.push(Predicate::from_value(item)?);
                }
                Ok(if tag == "and" {
                    Predicate::And(ps)
                } else {
                    Predicate::Or(ps)
                })
            }
            "not" => {
                let inner = obj
                    .get("clause")
                    .ok_or_else(|| "not: missing 'clause'".to_string())?;
                Ok(Predicate::Not(Box::new(Predicate::from_value(inner)?)))
            }
            other => Err(format!("unknown predicate tag: {other}")),
        }
    }

    /// Convenience: `serde_json::to_string` style.
    pub fn to_json_string(&self) -> String {
        self.to_value().to_string()
    }

    /// Convenience: `serde_json::from_str` style.
    pub fn from_json_str(s: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        Self::from_value(&v)
    }
}

impl Predicate {
    /// Whether the predicate references intent metadata. Used by
    /// the evaluator to decide whether it needs to load intent
    /// records for session resolution.
    fn needs_intent_resolution(&self) -> bool {
        match self {
            Predicate::Session { .. } => true,
            Predicate::And(ps) | Predicate::Or(ps) => {
                ps.iter().any(|p| p.needs_intent_resolution())
            }
            Predicate::Not(p) => p.needs_intent_resolution(),
            _ => false,
        }
    }

    /// Source of the candidate set. The evaluator narrows the
    /// log scan to the smallest candidate set across all clauses
    /// of an `And`, then filters within it. `All` is the universal
    /// candidate set ("every op in the log").
    fn candidate_root(&self) -> CandidateRoot {
        match self {
            Predicate::AncestorOf { op_id } => CandidateRoot::Ancestry(op_id.clone()),
            Predicate::And(ps) => {
                // Pick the most-restrictive root we can find. If any
                // clause restricts to an ancestry walk, prefer that
                // over scanning the whole log.
                ps.iter()
                    .map(|p| p.candidate_root())
                    .find(|r| matches!(r, CandidateRoot::Ancestry(_)))
                    .unwrap_or(CandidateRoot::All)
            }
            _ => CandidateRoot::All,
        }
    }
}

#[derive(Debug, Clone)]
enum CandidateRoot {
    All,
    Ancestry(OpId),
}

/// Resolver the evaluator uses to look up intent metadata when a
/// `Session` clause needs to know "which intents belong to this
/// session?". Wrapping it as a trait object lets test code stub it
/// without standing up a real [`crate::IntentLog`].
pub trait IntentResolver {
    /// Returns the session id of the given intent, if known.
    fn session_of(&self, intent_id: &IntentId) -> Option<SessionId>;
}

/// Evaluate a predicate against an op log. `Session` clauses are
/// resolved as if no intent had a session — most callers go
/// through [`evaluate_with_resolver`] instead. Use this entry
/// point when the predicate is known not to reference sessions.
pub fn evaluate(
    op_log: &OpLog,
    predicate: &Predicate,
) -> std::io::Result<Vec<OperationRecord>> {
    if predicate.needs_intent_resolution() {
        // Caller asked for a Session-touching predicate without
        // providing a resolver. Return an empty set for the
        // Session clauses; the rest of the predicate still works.
        evaluate_with_resolver(op_log, predicate, &NullResolver)
    } else {
        evaluate_with_resolver(op_log, predicate, &NullResolver)
    }
}

/// Evaluate with a caller-provided [`IntentResolver`] for `Session`
/// clauses. The returned vector is in the order the underlying
/// candidate scan produced — typically newest-first when the
/// candidate set is an ancestry walk, undefined when it's a full
/// log scan.
pub fn evaluate_with_resolver<R: IntentResolver + ?Sized>(
    op_log: &OpLog,
    predicate: &Predicate,
    resolver: &R,
) -> std::io::Result<Vec<OperationRecord>> {
    // Precompute every ancestry set referenced anywhere in the
    // predicate tree. `matches()` is then O(1) per record per
    // `AncestorOf` clause via set membership.
    let mut ancestries: std::collections::BTreeMap<OpId, std::collections::BTreeSet<OpId>> =
        std::collections::BTreeMap::new();
    collect_ancestor_ops(predicate, op_log, &mut ancestries)?;

    let candidates = candidate_set(op_log, &predicate.candidate_root())?;
    Ok(candidates
        .into_iter()
        .filter(|r| matches(r, predicate, resolver, &ancestries))
        .collect())
}

fn collect_ancestor_ops(
    predicate: &Predicate,
    op_log: &OpLog,
    out: &mut std::collections::BTreeMap<OpId, std::collections::BTreeSet<OpId>>,
) -> std::io::Result<()> {
    match predicate {
        Predicate::AncestorOf { op_id } if !out.contains_key(op_id) => {
            let set: std::collections::BTreeSet<OpId> = op_log
                .walk_back(op_id, None)?
                .into_iter()
                .map(|r| r.op_id)
                .collect();
            out.insert(op_id.clone(), set);
        }
        Predicate::AncestorOf { .. } => {}
        Predicate::And(ps) | Predicate::Or(ps) => {
            for p in ps {
                collect_ancestor_ops(p, op_log, out)?;
            }
        }
        Predicate::Not(p) => collect_ancestor_ops(p, op_log, out)?,
        _ => {}
    }
    Ok(())
}

fn candidate_set(
    op_log: &OpLog,
    root: &CandidateRoot,
) -> std::io::Result<Vec<OperationRecord>> {
    match root {
        CandidateRoot::Ancestry(head) => op_log.walk_back(head, None),
        CandidateRoot::All => op_log.list_all(),
    }
}

fn matches<R: IntentResolver + ?Sized>(
    rec: &OperationRecord,
    predicate: &Predicate,
    resolver: &R,
    ancestries: &std::collections::BTreeMap<OpId, std::collections::BTreeSet<OpId>>,
) -> bool {
    match predicate {
        Predicate::All => true,
        Predicate::Intent { intent_id } => {
            rec.op.intent_id.as_deref() == Some(intent_id)
        }
        Predicate::Session { session_id } => match &rec.op.intent_id {
            Some(id) => match resolver.session_of(id) {
                Some(s) => &s == session_id,
                None => false,
            },
            None => false,
        },
        Predicate::AncestorOf { op_id } => match ancestries.get(op_id) {
            Some(set) => set.contains(&rec.op_id),
            None => false,
        },
        Predicate::And(ps) => ps.iter().all(|p| matches(rec, p, resolver, ancestries)),
        Predicate::Or(ps) => ps.iter().any(|p| matches(rec, p, resolver, ancestries)),
        Predicate::Not(p) => !matches(rec, p, resolver, ancestries),
    }
}

/// Stub resolver used when [`evaluate`] is called without a real
/// resolver. Always returns `None`, so `Session` clauses match no
/// ops. Test code uses [`MapResolver`] (private to tests) for a
/// real lookup.
struct NullResolver;

impl IntentResolver for NullResolver {
    fn session_of(&self, _intent_id: &IntentId) -> Option<SessionId> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{Operation, OperationKind, StageTransition};
    use std::collections::{BTreeSet, HashMap};

    /// Test resolver backed by an in-memory map.
    struct MapResolver(HashMap<IntentId, SessionId>);

    impl IntentResolver for MapResolver {
        fn session_of(&self, intent_id: &IntentId) -> Option<SessionId> {
            self.0.get(intent_id).cloned()
        }
    }

    fn add_op_with_intent(sig: &str, stage: &str, intent: Option<&str>) -> OperationRecord {
        let mut op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stage.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
            },
            [],
        );
        if let Some(id) = intent {
            op = op.with_intent(id);
        }
        OperationRecord::new(
            op,
            StageTransition::Create {
                sig_id: sig.into(),
                stage_id: stage.into(),
            },
        )
    }

    fn modify_op_with_parent_and_intent(
        parent: &OpId,
        sig: &str,
        from: &str,
        to: &str,
        intent: Option<&str>,
    ) -> OperationRecord {
        let mut op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
                from_budget: None,
                to_budget: None,
            },
            [parent.clone()],
        );
        if let Some(id) = intent {
            op = op.with_intent(id);
        }
        OperationRecord::new(
            op,
            StageTransition::Replace {
                sig_id: sig.into(),
                from: from.into(),
                to: to.into(),
            },
        )
    }

    /// Three-op log: add (no intent) → modify A (intent X) →
    /// modify B (intent Y, child of modify A).
    fn three_op_log() -> (tempfile::TempDir, OpLog, [OpId; 3]) {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let r0 = add_op_with_intent("fn::Int->Int", "stage-0", None);
        let r1 = modify_op_with_parent_and_intent(
            &r0.op_id,
            "fn::Int->Int",
            "stage-0",
            "stage-1",
            Some("intent-X"),
        );
        let r2 = modify_op_with_parent_and_intent(
            &r1.op_id,
            "fn::Int->Int",
            "stage-1",
            "stage-2",
            Some("intent-Y"),
        );
        let ids = [r0.op_id.clone(), r1.op_id.clone(), r2.op_id.clone()];
        log.put(&r0).unwrap();
        log.put(&r1).unwrap();
        log.put(&r2).unwrap();
        (tmp, log, ids)
    }

    #[test]
    fn all_returns_every_op() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(&log, &Predicate::All).unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn intent_filters_by_intent_id() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(&log, &Predicate::Intent { intent_id: "intent-X".into() }).unwrap();
        assert_eq!(v.len(), 1, "exactly one op carries intent-X");
        assert_eq!(v[0].op.intent_id.as_deref(), Some("intent-X"));
    }

    #[test]
    fn intent_unknown_returns_empty() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(&log, &Predicate::Intent { intent_id: "unknown".into() }).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn ancestor_of_head_returns_full_ancestry() {
        let (_tmp, log, ids) = three_op_log();
        let head = ids[2].clone();
        let v = evaluate(&log, &Predicate::AncestorOf { op_id: head.clone() }).unwrap();
        assert_eq!(v.len(), 3, "head plus its 2 ancestors");
    }

    #[test]
    fn ancestor_of_middle_returns_two() {
        let (_tmp, log, ids) = three_op_log();
        let v = evaluate(&log, &Predicate::AncestorOf { op_id: ids[1].clone() }).unwrap();
        assert_eq!(v.len(), 2, "middle op plus its single ancestor");
    }

    #[test]
    fn and_intersects_clauses() {
        let (_tmp, log, ids) = three_op_log();
        // ops with intent-Y AND in the ancestry of head → just the
        // single intent-Y op.
        let head = ids[2].clone();
        let v = evaluate(
            &log,
            &Predicate::And(vec![
                Predicate::Intent { intent_id: "intent-Y".into() },
                Predicate::AncestorOf { op_id: head },
            ]),
        )
        .unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].op.intent_id.as_deref(), Some("intent-Y"));
    }

    #[test]
    fn and_with_disjoint_clauses_is_empty() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(
            &log,
            &Predicate::And(vec![
                Predicate::Intent { intent_id: "intent-X".into() },
                Predicate::Intent { intent_id: "intent-Y".into() },
            ]),
        )
        .unwrap();
        assert!(
            v.is_empty(),
            "no op carries both intents simultaneously",
        );
    }

    #[test]
    fn or_unions_clauses() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(
            &log,
            &Predicate::Or(vec![
                Predicate::Intent { intent_id: "intent-X".into() },
                Predicate::Intent { intent_id: "intent-Y".into() },
            ]),
        )
        .unwrap();
        assert_eq!(v.len(), 2, "two ops carry either intent");
    }

    #[test]
    fn not_inverts() {
        let (_tmp, log, _) = three_op_log();
        let v = evaluate(
            &log,
            &Predicate::Not(Box::new(Predicate::Intent { intent_id: "intent-X".into() })),
        )
        .unwrap();
        // 3 ops total, 1 carries intent-X, 2 don't.
        assert_eq!(v.len(), 2);
        assert!(v.iter().all(|r| r.op.intent_id.as_deref() != Some("intent-X")));
    }

    #[test]
    fn session_resolves_through_resolver() {
        let (_tmp, log, _) = three_op_log();
        // Map intent-X → session-A, intent-Y → session-B.
        let mut m = HashMap::new();
        m.insert("intent-X".to_string(), "session-A".to_string());
        m.insert("intent-Y".to_string(), "session-B".to_string());
        let resolver = MapResolver(m);

        let v = evaluate_with_resolver(
            &log,
            &Predicate::Session { session_id: "session-A".into() },
            &resolver,
        )
        .unwrap();
        assert_eq!(v.len(), 1, "exactly one op runs under session-A");
        assert_eq!(v[0].op.intent_id.as_deref(), Some("intent-X"));
    }

    #[test]
    fn session_with_unknown_id_returns_empty() {
        let (_tmp, log, _) = three_op_log();
        let resolver = MapResolver(HashMap::new());
        let v = evaluate_with_resolver(
            &log,
            &Predicate::Session { session_id: "unknown".into() },
            &resolver,
        )
        .unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn session_without_resolver_via_evaluate_returns_empty() {
        let (_tmp, log, _) = three_op_log();
        // `evaluate` (no resolver overload) treats Session as a
        // resolver-less query and returns nothing for it. This is
        // documented behavior — callers wanting Session resolution
        // must use `evaluate_with_resolver`.
        let v = evaluate(&log, &Predicate::Session { session_id: "session-A".into() }).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn predicate_round_trips_through_json_value() {
        let p = Predicate::And(vec![
            Predicate::Intent { intent_id: "i-X".into() },
            Predicate::Or(vec![
                Predicate::Session { session_id: "s-A".into() },
                Predicate::Not(Box::new(Predicate::All)),
            ]),
            Predicate::AncestorOf { op_id: "op-123".into() },
        ]);
        let s = p.to_json_string();
        let back = Predicate::from_json_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn from_json_str_rejects_unknown_tag() {
        let s = r#"{"predicate":"custom","whatever":1}"#;
        assert!(Predicate::from_json_str(s).is_err());
    }

    #[test]
    fn from_json_str_rejects_missing_field() {
        // intent_id is required for the `intent` variant.
        let s = r#"{"predicate":"intent"}"#;
        assert!(Predicate::from_json_str(s).is_err());
    }

    #[test]
    fn empty_log_returns_empty_for_all() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let v = evaluate(&log, &Predicate::All).unwrap();
        assert!(v.is_empty());
    }

    /// Smoke test: 100-op predicate eval finishes within a generous
    /// budget. Threshold is 5s rather than the ~50ms the engine
    /// actually achieves on dev hardware; CI runners with shared
    /// disk and constrained CPU are 10x slower than local on the
    /// IO-bound `evaluate` step (which reads 100 small JSON files).
    /// Asserting tighter than the runner's worst case turns this
    /// into a flake source rather than a regression alarm. A real
    /// perf regression (e.g. quadratic blow-up) still trips it.
    #[test]
    fn linear_scan_performance_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut prev: Option<OpId> = None;
        for i in 0..100 {
            let intent = if i % 3 == 0 { Some(format!("intent-{}", i % 5)) } else { None };
            let rec = match &prev {
                Some(p) => modify_op_with_parent_and_intent(
                    p,
                    &format!("fn-{i}"),
                    &format!("from-{i}"),
                    &format!("to-{i}"),
                    intent.as_deref(),
                ),
                None => add_op_with_intent(&format!("fn-{i}"), &format!("stage-{i}"), intent.as_deref()),
            };
            prev = Some(rec.op_id.clone());
            log.put(&rec).unwrap();
        }
        let start = std::time::Instant::now();
        let v = evaluate(&log, &Predicate::Intent { intent_id: "intent-2".into() }).unwrap();
        let elapsed = start.elapsed();
        assert!(!v.is_empty());
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "100-op predicate eval took {elapsed:?}, expected < 5s",
        );
    }

}
