//! M6 acceptance per spec §4.6.

use lex_ast::{canonicalize_program, sig_id, stage_id, Stage};
use lex_store::{Spec, StageStatus, Store, Test};
use lex_syntax::parse_source;
use tempfile::TempDir;

fn one_stage(src: &str, name: &str) -> Stage {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    stages.into_iter().find(|s| match s {
        Stage::FnDecl(fd) => fd.name == name,
        Stage::TypeDecl(td) => td.name == name,
        _ => false,
    }).expect("stage not found")
}

const FACTORIAL: &str = "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n";

#[test]
fn publishing_same_ast_twice_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let id1 = store.publish(&s).unwrap();
    let id2 = store.publish(&s).unwrap();
    assert_eq!(id1, id2, "same canonical AST ⇒ same StageId");
    assert_eq!(store.get_status(&id1).unwrap(), StageStatus::Draft);
}

#[test]
fn renaming_a_non_recursive_function_keeps_stage_id() {
    // §4.6 default: function name is in SigId but NOT in the implementation
    // hash that backs StageId. This test verifies the implementation-hash
    // half: a function that *doesn't* call itself by name has the same
    // body bytes regardless of its declared name, so renaming preserves
    // StageId. (Recursive renames also change body call-sites; that case
    // is a known tension flagged in spec §17 open question 4.)
    let s1 = one_stage("fn add(x :: Int, y :: Int) -> Int { x + y }\n", "add");
    let s2 = one_stage("fn plus(x :: Int, y :: Int) -> Int { x + y }\n", "plus");
    assert_eq!(stage_id(&s1).unwrap(), stage_id(&s2).unwrap(),
        "rename without body changes ⇒ same StageId");
    assert_ne!(sig_id(&s1).unwrap(), sig_id(&s2).unwrap(),
        "SigId includes the function name");
}

#[test]
fn activating_demotes_previous_active() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    // Two implementations of `add` with same SigId, different bodies.
    let v1 = one_stage("fn add(x :: Int, y :: Int) -> Int { x + y }\n", "add");
    let v2 = one_stage("fn add(x :: Int, y :: Int) -> Int { y + x }\n", "add");
    let id1 = store.publish(&v1).unwrap();
    let id2 = store.publish(&v2).unwrap();
    assert_ne!(id1, id2);

    let sig = sig_id(&v1).unwrap();
    assert_eq!(sig, sig_id(&v2).unwrap());

    store.activate(&id1).unwrap();
    assert_eq!(store.resolve_sig(&sig).unwrap().as_deref(), Some(&id1[..]));

    store.activate(&id2).unwrap();
    assert_eq!(store.resolve_sig(&sig).unwrap().as_deref(), Some(&id2[..]));
    assert_eq!(store.get_status(&id1).unwrap(), StageStatus::Deprecated);
    assert_eq!(store.get_status(&id2).unwrap(), StageStatus::Active);
}

#[test]
fn deprecate_then_tombstone_path() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let id = store.publish(&s).unwrap();
    store.activate(&id).unwrap();
    store.deprecate(&id, "obsolete").unwrap();
    assert_eq!(store.get_status(&id).unwrap(), StageStatus::Deprecated);
    store.tombstone(&id).unwrap();
    assert_eq!(store.get_status(&id).unwrap(), StageStatus::Tombstone);
}

#[test]
fn list_stages_by_name_returns_owning_sigs() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = one_stage(FACTORIAL, "factorial");
    let b = one_stage("fn add(x :: Int, y :: Int) -> Int { x + y }\n", "add");
    store.publish(&a).unwrap();
    store.publish(&b).unwrap();
    let sigs = store.list_stages_by_name("factorial").unwrap();
    assert_eq!(sigs, vec![sig_id(&a).unwrap()]);
}

#[test]
fn tests_and_specs_attach_to_sig() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let _ = store.publish(&s).unwrap();
    let sig = sig_id(&s).unwrap();
    store.attach_test(&sig, &Test {
        id: "fact5".into(),
        kind: "example".into(),
        input: serde_json::json!([5]),
        expected_output: serde_json::json!(120),
        effects_allowed: vec![],
    }).unwrap();
    store.attach_spec(&sig, &Spec {
        id: "fact_nonneg".into(),
        kind: "property".into(),
        body: serde_json::json!({"forall": "n >= 0", "expr": "factorial(n) >= 1"}),
    }).unwrap();
    let tests = store.list_tests(&sig).unwrap();
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].id, "fact5");
    let specs = store.list_specs(&sig).unwrap();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].id, "fact_nonneg");
}

#[test]
fn store_state_survives_reopen() {
    // §4.6: rebuilding from filesystem produces identical results.
    let tmp = TempDir::new().unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let sig = sig_id(&s).unwrap();
    let id = {
        let store = Store::open(tmp.path()).unwrap();
        let id = store.publish(&s).unwrap();
        store.activate(&id).unwrap();
        id
    };
    // New Store instance ⇒ no in-memory state. Filesystem must answer.
    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.resolve_sig(&sig).unwrap().as_deref(), Some(&id[..]));
    assert_eq!(store.get_status(&id).unwrap(), StageStatus::Active);
    let recovered = store.get_ast(&id).unwrap();
    assert_eq!(recovered, s);
}

#[test]
fn cannot_tombstone_active() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let id = store.publish(&s).unwrap();
    store.activate(&id).unwrap();
    let err = store.tombstone(&id).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Active") || msg.contains("Tombstone"),
        "expected invalid-transition message, got {msg}");
}
