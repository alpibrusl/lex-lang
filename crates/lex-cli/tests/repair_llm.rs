//! Conformance for `lex repair --apply` LLM path (#281 slice 2b).
//!
//! The live LLM call is short-circuited by setting
//! `LEX_REPAIR_LLM_FIXTURE` to a file whose contents replace
//! the model's response. This lets us exercise the end-to-end
//! flow — RepairHint → LLM call → transform parse → apply →
//! RepairAttempt — deterministically.

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

/// Seed a RepairHint by trying a deliberately ill-typed
/// `replace_match_arm` against `pick`. Returns the failed op_id
/// and the still-on-branch stage_id (which the LLM path will use
/// as `from_stage_id`).
fn seed_hint(store: &Store) -> (String, String) {
    let (_sig, from_stage_id) = publish_initial_pick(store);
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
        .unwrap();
    let failed_op_id = match &hint.kind {
        lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => failed_op_id.clone(),
        _ => unreachable!(),
    };
    (failed_op_id, from_stage_id)
}

#[test]
fn repair_apply_llm_path_lands_well_typed_fixture_response() {
    let (store, tmp) = fresh();
    let (failed_op_id, from_stage_id) = seed_hint(&store);

    // Write a fixture response: a well-typed replace_match_arm
    // transform against the branch-head stage. The LLM path
    // should parse, dispatch, and record a passed RepairAttempt.
    let fixture_json = serde_json::json!({
        "kind": "replace_match_arm",
        "from_stage_id": from_stage_id,
        "match_node": "n_0.2",
        "arm_index": 0,
        "new_body": { "node": "Literal", "value": { "kind": "Int", "value": 42 } }
    }).to_string();
    let fixture_path = tmp.path().join("fixture.json");
    std::fs::write(&fixture_path, &fixture_json).unwrap();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .env("LEX_REPAIR_LLM_FIXTURE", &fixture_path)
        .output().unwrap();
    assert!(out.status.success(),
        "stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "passed",
        "expected the well-typed transform to land; got {}", env);
    assert!(env["data"]["applied_op_id"].is_string());

    // RepairAttempt landed.
    let attlog = store.attestation_log().unwrap();
    let attempts: Vec<_> = attlog.list_all().unwrap().into_iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairAttempt { .. }))
        .collect();
    assert_eq!(attempts.len(), 1);
}

#[test]
fn repair_apply_llm_path_records_failure_on_malformed_fixture() {
    let (store, tmp) = fresh();
    let (failed_op_id, _from_stage_id) = seed_hint(&store);

    // Fixture is not JSON. The CLI should record a failed
    // RepairAttempt and emit the structured error.
    let fixture_path = tmp.path().join("fixture.json");
    std::fs::write(&fixture_path, "this is not json").unwrap();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .env("LEX_REPAIR_LLM_FIXTURE", &fixture_path)
        .output().unwrap();
    // Command exits 0 — the failure surfaces in the envelope.
    assert!(out.status.success());
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "failed");
}

#[test]
fn repair_apply_llm_path_records_failure_on_unknown_kind_fixture() {
    let (store, tmp) = fresh();
    let (failed_op_id, _from_stage_id) = seed_hint(&store);

    let fixture_path = tmp.path().join("fixture.json");
    std::fs::write(&fixture_path, r#"{"kind":"not_a_transform"}"#).unwrap();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .env("LEX_REPAIR_LLM_FIXTURE", &fixture_path)
        .output().unwrap();
    assert!(out.status.success());
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "failed");
    let err = env["data"]["error"].as_str().unwrap();
    assert!(err.contains("unknown transform kind"),
        "expected unknown-kind error, got: {err}");
}

#[test]
fn fixture_with_ill_typed_transform_records_failure() {
    let (store, tmp) = fresh();
    let (failed_op_id, from_stage_id) = seed_hint(&store);

    // Fixture: an ill-typed transform (string literal in arm
    // expecting Int). Parses cleanly but the apply path's
    // re-typecheck refuses it. Outcome = failed; RepairAttempt
    // records the failure.
    let fixture_json = serde_json::json!({
        "kind": "replace_match_arm",
        "from_stage_id": from_stage_id,
        "match_node": "n_0.2",
        "arm_index": 0,
        "new_body": { "node": "Literal", "value": { "kind": "Str", "value": "still wrong" } }
    }).to_string();
    let fixture_path = tmp.path().join("fixture.json");
    std::fs::write(&fixture_path, &fixture_json).unwrap();

    let out = std::process::Command::new(
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex")))
        .args([
            "--output", "json",
            "repair", &failed_op_id,
            "--apply",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .env("LEX_REPAIR_LLM_FIXTURE", &fixture_path)
        .output().unwrap();
    assert!(out.status.success());
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["outcome"], "failed");
}
