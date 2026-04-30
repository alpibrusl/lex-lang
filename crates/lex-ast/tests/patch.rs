//! Unit tests for `apply_patch`.

use lex_ast::{
    apply_patch, canonicalize_program, CExpr, CLit, Patch, PatchError, RecordField, Stage,
};
use lex_syntax::parse_source;

fn first_fn(src: &str) -> Stage {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    stages.into_iter().find(|s| matches!(s, Stage::FnDecl(_))).expect("fn")
}

fn fn_body(s: &Stage) -> &CExpr {
    match s {
        Stage::FnDecl(fd) => &fd.body,
        _ => panic!("not a fn"),
    }
}

#[test]
fn replace_a_literal_inside_a_let() {
    // body: let x := 1; x + 1
    // After canonicalization the body is a Let chain. Let's pick a
    // straightforward one: the function body is a literal we replace.
    let s = first_fn("fn one() -> Int { 1 }\n");
    // The body is `Literal { Int(1) }`. Path inside body: empty.
    // But apply_patch expects path[0] to be the body's position within
    // the stage. For a fn with no params, body sits at child index 1
    // (params take 0..0; return type is at 0; body is at 1).
    // → NodeId for the whole body is "n_0.1".
    let patch = Patch::Replace {
        target: "n_0.1".into(),
        with: CExpr::Literal { value: CLit::Int { value: 99 } },
    };
    let out = apply_patch(&s, &patch).expect("ok");
    assert!(matches!(fn_body(&out), CExpr::Literal { value: CLit::Int { value: 99 } }));
}

#[test]
fn replace_a_subexpression() {
    // fn add(x, y) -> x + y. Replace `x` with `42`.
    let s = first_fn("fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    // params occupy 0..2; return type at 2; body at 3 → body is "n_0.3".
    // body is BinOp { lhs: Var(x), rhs: Var(y) }. lhs is child 0 → "n_0.3.0".
    let patch = Patch::Replace {
        target: "n_0.3.0".into(),
        with: CExpr::Literal { value: CLit::Int { value: 42 } },
    };
    let out = apply_patch(&s, &patch).expect("ok");
    let body = fn_body(&out);
    match body {
        CExpr::BinOp { lhs, .. } => {
            assert!(matches!(**lhs, CExpr::Literal { value: CLit::Int { value: 42 } }));
        }
        other => panic!("expected BinOp, got {other:?}"),
    }
}

#[test]
fn delete_in_a_list_literal_removes_the_element() {
    // Canonicalization folds let-chains into nested Let nodes, so to
    // exercise Delete-from-list we use a list literal.
    let s = first_fn("fn xs() -> List[Int] { [10, 20, 30] }\n");
    // Body is at n_0.1 (no params). Body = ListLit { items: [10, 20, 30] }.
    // Delete element 1 (the `20`).
    let patch = Patch::Delete { target: "n_0.1.1".into() };
    let out = apply_patch(&s, &patch).expect("ok");
    match fn_body(&out) {
        CExpr::ListLit { items } => {
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0], CExpr::Literal { value: CLit::Int { value: 10 } }));
            assert!(matches!(items[1], CExpr::Literal { value: CLit::Int { value: 30 } }));
        }
        other => panic!("expected ListLit, got {other:?}"),
    }
}

#[test]
fn wrap_with_substitutes_the_hole() {
    // Wrap `x` with `Some(_HOLE_)` so the body becomes `Some(x)`.
    let s = first_fn("fn id(x :: Int) -> Option[Int] { Some(x) }\n");
    // Body at n_0.2 (1 param, return type at 1, body at 2).
    // Body is Constructor("Some", [Var x]). Inside: arg 0 → n_0.2.0.
    // Wrapper: a Tuple with _HOLE_ — the result becomes Some((x,)). Just
    // verify the substitution mechanic; type-correctness is the caller's.
    let patch = Patch::WrapWith {
        target: "n_0.2.0".into(),
        wrapper: CExpr::TupleLit {
            items: vec![
                CExpr::Var { name: "_HOLE_".into() },
                CExpr::Literal { value: CLit::Int { value: 7 } },
            ],
        },
    };
    let out = apply_patch(&s, &patch).expect("ok");
    match fn_body(&out) {
        CExpr::Constructor { name, args } => {
            assert_eq!(name, "Some");
            match &args[0] {
                CExpr::TupleLit { items } => {
                    assert_eq!(items.len(), 2);
                    assert!(matches!(items[0], CExpr::Var { ref name } if name == "x"));
                    assert!(matches!(items[1], CExpr::Literal { value: CLit::Int { value: 7 } }));
                }
                other => panic!("expected TupleLit, got {other:?}"),
            }
        }
        other => panic!("expected Constructor, got {other:?}"),
    }
}

#[test]
fn wrap_with_rejects_missing_hole() {
    let s = first_fn("fn one() -> Int { 1 }\n");
    let patch = Patch::WrapWith {
        target: "n_0.1".into(),
        wrapper: CExpr::Literal { value: CLit::Int { value: 0 } },
    };
    let err = apply_patch(&s, &patch).unwrap_err();
    assert!(matches!(err, PatchError::WrapWithMissingHole));
}

#[test]
fn unknown_node_id_returns_structured_error() {
    let s = first_fn("fn one() -> Int { 1 }\n");
    let patch = Patch::Replace {
        target: "n_0.99.99".into(),
        with: CExpr::Literal { value: CLit::Int { value: 0 } },
    };
    let err = apply_patch(&s, &patch).unwrap_err();
    assert!(matches!(err, PatchError::UnknownNode { .. }));
}

#[test]
fn malformed_node_id_rejected() {
    let s = first_fn("fn one() -> Int { 1 }\n");
    let patch = Patch::Replace {
        target: "not-a-node".into(),
        with: CExpr::Literal { value: CLit::Int { value: 0 } },
    };
    assert!(matches!(apply_patch(&s, &patch).unwrap_err(), PatchError::BadNodeId(_)));
}

#[test]
fn cannot_replace_stage_root() {
    let s = first_fn("fn one() -> Int { 1 }\n");
    let patch = Patch::Replace {
        target: "n_0".into(),
        with: CExpr::Literal { value: CLit::Int { value: 0 } },
    };
    let err = apply_patch(&s, &patch).unwrap_err();
    assert!(matches!(err, PatchError::NonExprTarget { .. }));
}

#[test]
fn replace_inside_a_record_literal_field() {
    let s = first_fn("fn r() -> { x :: Int, y :: Int } { { x: 1, y: 2 } }\n");
    // Body at n_0.1. RecordLit fields are sorted alphabetically by
    // canonicalization: [x, y]. Field 0's value (x: 1) is at n_0.1.0.
    let patch = Patch::Replace {
        target: "n_0.1.0".into(),
        with: CExpr::Literal { value: CLit::Int { value: 99 } },
    };
    let out = apply_patch(&s, &patch).expect("ok");
    match fn_body(&out) {
        CExpr::RecordLit { fields } => {
            // First field after sort is `x`.
            let first: &RecordField = &fields[0];
            assert_eq!(first.name, "x");
            assert!(matches!(first.value, CExpr::Literal { value: CLit::Int { value: 99 } }));
        }
        other => panic!("expected RecordLit, got {other:?}"),
    }
}
