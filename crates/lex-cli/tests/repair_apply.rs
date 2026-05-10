//! End-to-end conformance for `lex repair --apply --transform` (#281
//! slice 2a). Tests the full hint → apply → RepairAttempt flow.
//!
//! Run in-process against the Store API rather than as a CLI
//! subprocess because the transform JSON's CExpr payload is
//! cleanest constructed in Rust.

use lex_ast::{canonicalize_program, sig_id, stage_id, CExpr, CLit, NodeId, Stage};
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn stage_named(src: &str, name: &str) -> Stage {
    let prog = parse_source(src).unwrap();
    canonicalize_program(&prog)
        .into_iter()
        .find(|s| match s {
            Stage::FnDecl(fd) => fd.name == name,
            _ => false,
        })
        .expect("stage not found")
}

const PICK_SRC: &str = "fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }\n";

fn publish_initial_pick(store: &Store) -> (String, String) {
    let s = stage_named(PICK_SRC, "pick");
    let sig = sig_id(&s).unwrap();
    let stg = stage_id(&s).unwrap();
    store.publish(&s).unwrap();
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: Default::default(),
            budget_cost: None,
        },
        [],
    );
    let t = lex_vcs::StageTransition::Create {
        sig_id: sig.clone(),
        stage_id: stg.clone(),
    };
    store.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
    (sig, stg)
}

/// Trigger a TypeError that emits a RepairHint, then apply a
/// well-typed transform against the original stage. The
/// RepairAttempt records the success.
#[test]
fn repair_apply_with_well_typed_transform_lands_and_emits_repair_attempt() {
    let (store, tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Step 1 — provoke a RepairHint via an ill-typed transform.
    let ill_typed = CExpr::Literal { value: CLit::Str { value: "boom".into() } };
    let err = store
        .apply_replace_match_arm(
            DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
            0, ill_typed,
        )
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TypeError(_)));

    let attlog = store.attestation_log().unwrap();
    let hint = attlog.list_all().unwrap().into_iter()
        .find(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairHint { .. }))
        .expect("RepairHint should land on the ill-typed attempt");
    let failed_op_id = match &hint.kind {
        lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => failed_op_id.clone(),
        _ => unreachable!(),
    };

    // Step 2 — drive the CLI's `repair --apply --transform` via
    // a subprocess so the integration covers the JSON parser.
    let transform_json = serde_json::json!({
        "kind": "replace_match_arm",
        "from_stage_id": from_stage_id,
        "match_node": "n_0.2",
        "arm_index": 0,
        "new_body": { "node": "Literal", "value": { "kind": "Int", "value": 42 } }
    }).to_string();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply", "--transform", &transform_json,
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(),
        "repair --apply failed: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "passed");
    assert!(env["data"]["applied_op_id"].is_string(),
        "applied_op_id should be set on success");

    // Step 3 — RepairAttempt attestation lands.
    let post = attlog.list_all().unwrap();
    let attempts: Vec<_> = post.iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairAttempt { .. }))
        .collect();
    assert_eq!(attempts.len(), 1, "exactly one RepairAttempt should land");
    let lex_vcs::AttestationKind::RepairAttempt { hint_id, outcome, applied_op_id } =
        &attempts[0].kind else { unreachable!() };
    assert_eq!(hint_id, &hint.attestation_id);
    assert_eq!(outcome, "passed");
    assert!(applied_op_id.is_some());
}

#[test]
fn repair_apply_with_ill_typed_transform_records_failure() {
    let (store, tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Seed a hint.
    let _ = store.apply_replace_match_arm(
        DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
        0, CExpr::Literal { value: CLit::Str { value: "x".into() } },
    );

    let attlog = store.attestation_log().unwrap();
    let hint = attlog.list_all().unwrap().into_iter()
        .find(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairHint { .. }))
        .unwrap();
    let failed_op_id = match &hint.kind {
        lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => failed_op_id.clone(),
        _ => unreachable!(),
    };

    // Apply another ill-typed transform — still a string literal.
    let transform_json = serde_json::json!({
        "kind": "replace_match_arm",
        "from_stage_id": from_stage_id,
        "match_node": "n_0.2",
        "arm_index": 0,
        "new_body": { "node": "Literal", "value": { "kind": "Str", "value": "still wrong" } }
    }).to_string();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply", "--transform", &transform_json,
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    // The command itself succeeds — the failure outcome lives in
    // the envelope. Exit code reflects "did the command run", not
    // "did the repair land."
    assert!(out.status.success(),
        "command ran cleanly; outcome lives in the envelope: stderr={}",
        String::from_utf8_lossy(&out.stderr));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "failed");

    // RepairAttempt with failed outcome should still land — the
    // audit trail records the attempt regardless of outcome.
    let post = attlog.list_all().unwrap();
    let attempts: Vec<_> = post.iter()
        .filter_map(|a| match &a.kind {
            lex_vcs::AttestationKind::RepairAttempt { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
        .collect();
    assert!(attempts.iter().any(|o| o == "failed"),
        "a 'failed' RepairAttempt should land; got {attempts:?}");
}

#[test]
fn repair_apply_rejects_unknown_transform_kind() {
    let (store, tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Seed a hint.
    let _ = store.apply_replace_match_arm(
        DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
        0, CExpr::Literal { value: CLit::Str { value: "x".into() } },
    );

    let attlog = store.attestation_log().unwrap();
    let hint = attlog.list_all().unwrap().into_iter()
        .find(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairHint { .. }))
        .unwrap();
    let failed_op_id = match &hint.kind {
        lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => failed_op_id.clone(),
        _ => unreachable!(),
    };

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply", "--transform", r#"{"kind":"not_a_real_transform"}"#,
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "command ran cleanly");
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "failed");
    assert!(env["data"]["error"].as_str().unwrap().contains("unknown transform kind"),
        "envelope should describe the kind error, got: {}", env["data"]["error"]);
}
