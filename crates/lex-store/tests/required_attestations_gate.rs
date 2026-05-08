//! Conformance tests for #245's `required_attestations` gate.
//!
//! The gate is the agent-shaped equivalent of "branch protection
//! rules": each entry in `policy.required_attestations` says "every
//! op landed on this branch must carry a `Passed` attestation of
//! kind X (sometimes only when its effects intersect Y) before the
//! branch head can advance past it."
//!
//! These tests cover:
//!
//! 1. Default-permissive: no policy file → no gate, normal apply works.
//! 2. Policy requires `TypeCheck` → succeeds because
//!    `apply_operation_checked` auto-emits TypeCheck before the gate.
//! 3. Policy requires `Spec` → fails (the issue's explicit acceptance
//!    case); branch head unchanged; structured envelope.
//! 4. `effects_intersect` clause fires only when the op's declared
//!    effects intersect.
//! 5. Multiple rules surface every missing kind in a single envelope.

use lex_ast::canonicalize_program;
use lex_store::{Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH};
use lex_store::policy::{
    AttestationCondition, BranchAdvanceBlocked, PolicyFile, RequiredAttestationKind,
};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    let prog = parse_source(src).expect("parse");
    canonicalize_program(&prog)
}

fn pure_fn_op() -> (Operation, StageTransition) {
    (
        Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac".into(),
                stage_id: "stg-1".into(),
                effects: BTreeSet::new(),
            },
            [],
        ),
        StageTransition::Create {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
        },
    )
}

fn io_fn_op() -> (Operation, StageTransition) {
    let mut effects: BTreeSet<String> = BTreeSet::new();
    effects.insert("io".into());
    (
        Operation::new(
            OperationKind::AddFunction {
                sig_id: "log".into(),
                stage_id: "stg-log-1".into(),
                effects,
            },
            [],
        ),
        StageTransition::Create {
            sig_id: "log".into(),
            stage_id: "stg-log-1".into(),
        },
    )
}

fn pure_candidate() -> Vec<lex_ast::Stage> {
    parse("fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n")
}

#[test]
fn no_policy_file_means_no_gate() {
    let (s, _tmp) = fresh();
    let (op, t) = pure_fn_op();
    s.apply_operation_checked(DEFAULT_BRANCH, op, t, &pure_candidate())
        .expect("apply must succeed when no policy.json exists");
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert!(b.head_op.is_some(), "branch head must advance");
}

#[test]
fn policy_requiring_typecheck_passes_because_it_is_auto_emitted() {
    let (s, _tmp) = fresh();
    let mut policy = PolicyFile::default();
    policy.require_attestation(
        RequiredAttestationKind::TypeCheck,
        AttestationCondition::Always,
    );
    lex_store::policy::save(s.root(), &policy).unwrap();

    let (op, t) = pure_fn_op();
    s.apply_operation_checked(DEFAULT_BRANCH, op, t, &pure_candidate())
        .expect("TypeCheck is auto-emitted before the gate runs");
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert!(b.head_op.is_some());
}

#[test]
fn policy_requiring_spec_blocks_the_advance_when_no_spec_attestation_exists() {
    // The issue's explicit acceptance case: publish an op without a
    // `Spec` attestation, set policy to require `Spec`, assert
    // branch advance fails with the expected envelope.
    let (s, _tmp) = fresh();
    let mut policy = PolicyFile::default();
    policy.require_attestation(
        RequiredAttestationKind::Spec,
        AttestationCondition::Always,
    );
    lex_store::policy::save(s.root(), &policy).unwrap();

    let (op, t) = pure_fn_op();
    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &pure_candidate())
        .expect_err("Spec is missing — gate must refuse");
    let blocked = match err {
        StoreError::BranchAdvanceBlocked(b) => b,
        other => panic!("expected BranchAdvanceBlocked, got {other:?}"),
    };
    assert_eq!(blocked.stage_id.as_deref(), Some("stg-1"));
    assert_eq!(blocked.missing, vec!["spec".to_string()]);

    // Branch head must NOT have advanced. The op record on disk
    // *did* get persisted (the gate runs after `lex_vcs::apply`),
    // which is the documented two-phase behavior — re-running with
    // the missing Spec attestation recorded will succeed without
    // re-persisting the op.
    let branch = s.get_branch(DEFAULT_BRANCH).unwrap();
    assert!(
        branch.is_none() || branch.unwrap().head_op.is_none(),
        "branch head must not advance when the gate refuses",
    );

    // Envelope shape — the structure agent runtimes will see on
    // the HTTP API surface.
    let env = blocked.to_envelope();
    assert_eq!(env["error"], "BranchAdvanceBlocked");
    assert_eq!(env["op_id"], serde_json::Value::String(blocked.op_id.clone()));
    assert_eq!(env["stage_id"], "stg-1");
    assert_eq!(env["missing"], serde_json::json!(["spec"]));
}

#[test]
fn effects_intersect_rule_does_not_fire_on_pure_op() {
    // SandboxRun is required only when the op's effects intersect
    // [io, fs_write]. A pure function (empty effect set) doesn't
    // trigger the rule, so the apply succeeds even though no
    // SandboxRun attestation exists.
    let (s, _tmp) = fresh();
    let mut policy = PolicyFile::default();
    let mut effects: BTreeSet<String> = BTreeSet::new();
    effects.insert("io".into());
    effects.insert("fs_write".into());
    policy.require_attestation(
        RequiredAttestationKind::SandboxRun,
        AttestationCondition::EffectsIntersect(effects),
    );
    lex_store::policy::save(s.root(), &policy).unwrap();

    let (op, t) = pure_fn_op();
    s.apply_operation_checked(DEFAULT_BRANCH, op, t, &pure_candidate())
        .expect("rule must not fire on a pure op");
}

#[test]
fn effects_intersect_rule_fires_on_io_op() {
    // Same rule, but now the op declares `[io]`. The intersection
    // is non-empty → the rule fires → no SandboxRun attestation
    // exists → the gate refuses.
    let (s, _tmp) = fresh();
    let mut policy = PolicyFile::default();
    let mut effects: BTreeSet<String> = BTreeSet::new();
    effects.insert("io".into());
    effects.insert("fs_write".into());
    policy.require_attestation(
        RequiredAttestationKind::SandboxRun,
        AttestationCondition::EffectsIntersect(effects),
    );
    lex_store::policy::save(s.root(), &policy).unwrap();

    // Candidate declares the `[io]` effect on its return signature.
    // Body content is irrelevant for the gate test — the effect
    // declaration is what triggers the rule.
    let candidate = parse("fn log(x :: Int) -> [io] Int { x }\n");
    let (op, t) = io_fn_op();
    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect_err("io op without SandboxRun must be refused");
    let blocked: BranchAdvanceBlocked = match err {
        StoreError::BranchAdvanceBlocked(b) => b,
        other => panic!("expected BranchAdvanceBlocked, got {other:?}"),
    };
    assert_eq!(blocked.missing, vec!["sandbox_run".to_string()]);
}

#[test]
fn multiple_required_kinds_surface_every_missing_one() {
    let (s, _tmp) = fresh();
    let mut policy = PolicyFile::default();
    policy.require_attestation(
        RequiredAttestationKind::Spec,
        AttestationCondition::Always,
    );
    policy.require_attestation(
        RequiredAttestationKind::Examples,
        AttestationCondition::Always,
    );
    policy.require_attestation(
        RequiredAttestationKind::TypeCheck,
        AttestationCondition::Always,
    );
    lex_store::policy::save(s.root(), &policy).unwrap();

    let (op, t) = pure_fn_op();
    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &pure_candidate())
        .expect_err("Spec and Examples are missing");
    let blocked = match err {
        StoreError::BranchAdvanceBlocked(b) => b,
        other => panic!("expected BranchAdvanceBlocked, got {other:?}"),
    };
    // TypeCheck is auto-emitted, so it shouldn't appear in
    // `missing`. The other two should.
    assert!(!blocked.missing.contains(&"type_check".to_string()));
    assert!(blocked.missing.contains(&"spec".to_string()));
    assert!(blocked.missing.contains(&"examples".to_string()));
}

#[test]
fn unrequire_attestation_clears_the_rule() {
    let mut policy = PolicyFile::default();
    policy.require_attestation(
        RequiredAttestationKind::Spec,
        AttestationCondition::Always,
    );
    policy.require_attestation(
        RequiredAttestationKind::Spec,
        AttestationCondition::EffectsIntersect(
            ["io".to_string()].into_iter().collect(),
        ),
    );
    assert_eq!(policy.required_attestations.len(), 2);
    let removed = policy.unrequire_attestation(RequiredAttestationKind::Spec);
    assert_eq!(removed, 2, "both Spec rules should be dropped");
    assert!(policy.required_attestations.is_empty());
}

#[test]
fn legacy_policy_json_without_required_attestations_field_loads_cleanly() {
    // Backward-compat: a policy.json from before #245 has no
    // `required_attestations` field. It must deserialize with an
    // empty Vec (serde default) — same behavior as no policy file.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("policy.json"),
        r#"{ "blocked_producers": [{"tool":"old-bot","reason":"x","blocked_at":1000}] }"#,
    )
    .unwrap();
    let policy = lex_store::policy::load(tmp.path()).unwrap().unwrap();
    assert_eq!(policy.blocked_producers.len(), 1);
    assert!(policy.required_attestations.is_empty());
}
