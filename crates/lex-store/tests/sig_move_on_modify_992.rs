//! #992, second route to the same damage: a *signature* change that is not an
//! effect change.
//!
//! #993 taught `ChangeEffectSig` to move the head entry to the new SigId. But
//! a SigId covers more than the effect row — the input and output types and
//! the signature-level `examples` too (and a type's params). The publish diff
//! keys declarations by name, so changing any of those reads as a plain
//! modification and was emitted as `ModifyBody` / `ModifyType` under the
//! **old** sig: `Replace (old_sig, new_stage)` — the exact unsatisfiable pair
//! #992 is about, since the new AST is filed under the sig it hashes to.
//!
//! Found by the write-time gate (`unsatisfiable_pair_gate_992.rs`), which
//! refused lex-api's own `modifying_a_function_in_place_is_one_modify_not_an_add`
//! publish: bumping an `examples` value moved the sig.
//!
//! As in `effect_sig_move_992.rs`, these assert the head map and the AST read,
//! not the op's fields, because that is what rendering and a release consume.

use std::collections::BTreeMap;

use lex_ast::canonicalize_program;
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

fn stages_of(src: &str) -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(src).expect("parse"))
}

/// Publish `src` as the whole program, diffed against the current head, and
/// return the resulting head map.
fn publish(store: &Store, src: &str) -> BTreeMap<String, String> {
    let stages = stages_of(src);
    let (new_fns, new_types) = split(stages.iter().cloned());
    let head = store.branch_head(DEFAULT_BRANCH).expect("head");
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    let (old_fns, old_types) = split(
        store
            .get_asts_for_sigs_bulk(&pairs)
            .into_iter()
            .map(|a| a.expect("every head pair must resolve before a publish")),
    );
    let diff = lex_vcs::compute_diff_with_types(&old_fns, &new_fns, &old_types, &new_types, true);
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &BTreeMap::new(), true)
        .expect("publish");
    store.branch_head(DEFAULT_BRANCH).expect("head")
}

type Split = (
    BTreeMap<String, lex_ast::FnDecl>,
    BTreeMap<String, lex_ast::TypeDecl>,
);

fn split(stages: impl Iterator<Item = lex_ast::Stage>) -> Split {
    let mut fns = BTreeMap::new();
    let mut types = BTreeMap::new();
    for st in stages {
        match st {
            lex_ast::Stage::FnDecl(fd) => {
                fns.insert(fd.name.clone(), fd);
            }
            lex_ast::Stage::TypeDecl(td) => {
                types.insert(td.name.clone(), td);
            }
            lex_ast::Stage::Import(_) => {}
        }
    }
    (fns, types)
}

fn sig_of(src: &str, name: &str) -> String {
    let st = stages_of(src)
        .into_iter()
        .find(|s| match s {
            lex_ast::Stage::FnDecl(fd) => fd.name == name,
            lex_ast::Stage::TypeDecl(td) => td.name == name,
            lex_ast::Stage::Import(_) => false,
        })
        .expect("declaration");
    lex_ast::sig_id(&st).expect("sig")
}

fn store() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

/// Every pair the head names resolves to an AST — the property a render needs.
fn assert_head_resolves(store: &Store, head: &BTreeMap<String, String>) {
    let pairs: Vec<(String, String)> = head.clone().into_iter().collect();
    for ((sig, stage), ast) in pairs.iter().zip(store.get_asts_for_sigs_bulk(&pairs)) {
        assert!(
            ast.is_ok(),
            "head names ({sig}, {stage}) but no AST is filed there: {ast:?}"
        );
    }
}

/// Publish `before`, then `after`, and assert the declaration moved cleanly
/// from its old sig to its new one.
fn assert_moves(before: &str, after: &str, name: &str) {
    let (old_sig, new_sig) = (sig_of(before, name), sig_of(after, name));
    assert_ne!(old_sig, new_sig, "premise: this change must move the SigId");

    let (store, _tmp) = store();
    let head = publish(&store, before);
    assert!(head.contains_key(&old_sig), "baseline: {head:?}");
    let count = head.len();

    let head = publish(&store, after);
    assert!(
        head.contains_key(&new_sig),
        "the head must bind the new sig: {head:?}"
    );
    assert!(
        !head.contains_key(&old_sig),
        "and must drop the old one: {head:?}"
    );
    assert_eq!(
        head.len(),
        count,
        "one declaration, one head entry: {head:?}"
    );
    assert_head_resolves(&store, &head);
}

/// The shape lex-api's own test hit: only a signature-level example changed.
#[test]
fn changing_an_example_moves_the_head_entry() {
    assert_moves(
        "fn counter() -> Int\n  examples {\n    counter() => 1,\n  }\n{ 1 }\n",
        "fn counter() -> Int\n  examples {\n    counter() => 2,\n  }\n{ 2 }\n",
        "counter",
    );
}

#[test]
fn changing_a_parameter_type_moves_the_head_entry() {
    assert_moves(
        "fn f(x :: Int) -> Int { 1 }\n",
        "fn f(x :: Str) -> Int { 1 }\n",
        "f",
    );
}

#[test]
fn changing_the_return_type_moves_the_head_entry() {
    assert_moves("fn f() -> Int { 1 }\n", "fn f() -> Str { \"a\" }\n", "f");
}

/// A type's SigId covers its params, so adding one is a sig move too.
#[test]
fn changing_a_type_parameter_moves_the_head_entry() {
    assert_moves(
        "type Box = { v :: Int }\n",
        "type Box[T] = { v :: T }\n",
        "Box",
    );
}

/// Negative control: a body-only change keeps its sig, and must stay the
/// plain in-place `Replace` it always was.
#[test]
fn a_body_only_change_keeps_its_sig() {
    let before = "fn f(x :: Int) -> Int { x }\n";
    let after = "fn f(x :: Int) -> Int { x + 1 }\n";
    let sig = sig_of(before, "f");
    assert_eq!(
        sig,
        sig_of(after, "f"),
        "premise: a body change keeps the sig"
    );

    let (store, _tmp) = store();
    publish(&store, before);
    let head = publish(&store, after);
    assert_eq!(head.keys().collect::<Vec<_>>(), vec![&sig], "{head:?}");
    assert_head_resolves(&store, &head);
}

/// A sig move must carry the declaration's source file with it. Rendering
/// decides single- vs multi-module by whether *every* head sig records an
/// `in_file`; a moved sig that dropped it would silently collapse a
/// multi-module package into one file.
#[test]
fn a_sig_move_keeps_the_declarations_source_file() {
    use lex_store::{Operation, OperationKind};
    let (store, _tmp) = store();
    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let put = |op: Operation| {
        let t = lex_store::transition_for_kind(&op.kind);
        let rec = lex_vcs::OperationRecord::new(op, t);
        let id = rec.op_id.clone();
        log.put(&rec).unwrap();
        id
    };
    let a = put(Operation::new(
        OperationKind::AddFunction {
            sig_id: "old".into(),
            stage_id: "s1".into(),
            effects: Default::default(),
            budget_cost: None,
            in_file: Some("src/web.lex".into()),
        },
        vec![],
    ));
    let b = put(Operation::new(
        OperationKind::ModifyBody {
            sig_id: "old".into(),
            from_stage_id: "s1".into(),
            to_stage_id: "s2".into(),
            from_budget: None,
            to_budget: None,
            to_sig_id: Some("new".into()),
        },
        vec![a],
    ));
    let head = lex_store::render::package_head_at_op(&store, &b).unwrap();
    assert_eq!(
        head.map.into_iter().collect::<Vec<_>>(),
        vec![("new".to_string(), "s2".to_string())]
    );
    assert_eq!(
        head.sig_files.get("new").map(String::as_str),
        Some("src/web.lex")
    );
    assert!(!head.sig_files.contains_key("old"));
}
