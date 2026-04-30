//! M9 Phase 1 acceptance.
//!
//! Phase 1 covers spec §13.7 acceptance items #3 (shape mismatch
//! compile error) and structural plumbing for #2 (Core stage callable
//! from Lex). Items #1 (matmul perf) and #4 (mut return error) belong
//! to Phase 2 (Cranelift native codegen + mutation analysis).

use core_compiler::{
    check::{check_core_stage, matrix, CoreStage, CoreType, SizedNumeric},
    shape::{ShapeExpr, ShapeSolver, Tensor},
    error::CoreError,
};
use lex_ast::canonicalize_program;
use lex_syntax::parse_source;

fn make_stage(src: &str, name: &str) -> lex_ast::Stage {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    stages.into_iter().find(|s| match s {
        lex_ast::Stage::FnDecl(fd) => fd.name == name,
        _ => false,
    }).expect("stage")
}

#[test]
fn shape_solver_unifies_matching_dims() {
    let mut s = ShapeSolver::new();
    s.unify(&ShapeExpr::lit(5), &ShapeExpr::lit(5)).unwrap();
    s.unify(&ShapeExpr::var("M"), &ShapeExpr::lit(7)).unwrap();
    let r = s.resolve(&ShapeExpr::var("M"));
    assert_eq!(r, ShapeExpr::lit(7));
}

#[test]
fn shape_solver_rejects_mismatch() {
    let mut s = ShapeSolver::new();
    let err = s.unify(&ShapeExpr::lit(5), &ShapeExpr::lit(7)).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("dimension mismatch"), "got: {msg}");
}

#[test]
fn shape_solver_folds_constants() {
    let s = ShapeSolver::new();
    let e = ShapeExpr::sum(ShapeExpr::lit(3), ShapeExpr::lit(4));
    assert_eq!(s.resolve(&e), ShapeExpr::lit(7));
    let e2 = ShapeExpr::product(ShapeExpr::lit(3), ShapeExpr::lit(4));
    assert_eq!(s.resolve(&e2), ShapeExpr::lit(12));
}

#[test]
fn shape_solver_folds_through_var_bindings() {
    let mut s = ShapeSolver::new();
    s.unify(&ShapeExpr::var("M"), &ShapeExpr::lit(2)).unwrap();
    s.unify(&ShapeExpr::var("N"), &ShapeExpr::lit(3)).unwrap();
    let e = ShapeExpr::product(ShapeExpr::var("M"), ShapeExpr::var("N"));
    assert_eq!(s.resolve(&e), ShapeExpr::lit(6));
}

#[test]
fn matmul_signature_with_matching_inner_dim_typechecks() {
    // matmul[M, K, N](a :: Matrix[M, K, F64], b :: Matrix[K, N, F64]) -> Matrix[M, N, F64]
    let stage_src = "fn matmul(a :: Float, b :: Float) -> Float { a * b }\n";
    let stage = make_stage(stage_src, "matmul");
    let cs = CoreStage {
        stage,
        type_params: vec!["M".into(), "K".into(), "N".into()],
        param_types: vec![
            CoreType::Tensor(matrix(ShapeExpr::var("M"), ShapeExpr::var("K"), "F64")),
            CoreType::Tensor(matrix(ShapeExpr::var("K"), ShapeExpr::var("N"), "F64")),
        ],
        return_type: CoreType::Tensor(matrix(ShapeExpr::var("M"), ShapeExpr::var("N"), "F64")),
    };
    let r = check_core_stage(cs);
    assert!(r.is_ok(), "matmul with matching inner dim must typecheck: {:?}", r.err());
}

#[test]
fn matmul_signature_with_mismatched_inner_dim_is_rejected() {
    // matmul: Matrix[M, 4, F64] x Matrix[5, N, F64] -> Matrix[M, N, F64]
    // Inner dims 4 vs 5 — must error.
    let stage_src = "fn matmul(a :: Float, b :: Float) -> Float { a * b }\n";
    let stage = make_stage(stage_src, "matmul");
    let cs = CoreStage {
        stage,
        type_params: vec!["M".into(), "N".into()],
        param_types: vec![
            CoreType::Tensor(matrix(ShapeExpr::var("M"), ShapeExpr::lit(4), "F64")),
            CoreType::Tensor(matrix(ShapeExpr::lit(5), ShapeExpr::var("N"), "F64")),
        ],
        return_type: CoreType::Tensor(matrix(ShapeExpr::var("M"), ShapeExpr::var("N"), "F64")),
    };
    let errs = check_core_stage(cs).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, CoreError::ShapeMismatch { .. })),
        "expected shape_mismatch, got {errs:#?}");
}

#[test]
fn matmul_signature_with_concrete_correct_dims() {
    // 1024×512 · 512×768 -> 1024×768  ✓
    let stage_src = "fn matmul(a :: Float, b :: Float) -> Float { a * b }\n";
    let stage = make_stage(stage_src, "matmul");
    let cs = CoreStage {
        stage,
        type_params: vec![],
        param_types: vec![
            CoreType::Tensor(matrix(ShapeExpr::lit(1024), ShapeExpr::lit(512), "F64")),
            CoreType::Tensor(matrix(ShapeExpr::lit(512), ShapeExpr::lit(768), "F64")),
        ],
        return_type: CoreType::Tensor(matrix(ShapeExpr::lit(1024), ShapeExpr::lit(768), "F64")),
    };
    assert!(check_core_stage(cs).is_ok());
}

#[test]
fn unknown_dtype_is_rejected() {
    let stage_src = "fn id(x :: Float) -> Float { x }\n";
    let stage = make_stage(stage_src, "id");
    let cs = CoreStage {
        stage,
        type_params: vec![],
        param_types: vec![CoreType::Tensor(Tensor {
            shape: vec![ShapeExpr::lit(3)],
            dtype: "X99".into(),
        })],
        return_type: CoreType::Tensor(Tensor {
            shape: vec![ShapeExpr::lit(3)],
            dtype: "X99".into(),
        }),
    };
    let errs = check_core_stage(cs).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, CoreError::UnknownDtype { .. })));
}

#[test]
fn rank_mismatch_in_solver() {
    let mut s = ShapeSolver::new();
    let a = vec![ShapeExpr::lit(3), ShapeExpr::lit(4)];
    let b = vec![ShapeExpr::lit(3)];
    let err = s.unify_shapes(&a, &b).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("rank mismatch"));
}

#[test]
fn sized_numeric_round_trip() {
    for s in &["U8", "U16", "U32", "U64", "I8", "I16", "I32", "I64", "F32", "F64"] {
        let parsed = SizedNumeric::parse(s).expect("recognized");
        assert_eq!(parsed.name(), *s);
    }
    assert!(SizedNumeric::parse("Q42").is_none());
    assert!(SizedNumeric::F32.is_float());
    assert!(SizedNumeric::F64.is_float());
    assert!(!SizedNumeric::I32.is_float());
}

#[test]
fn matmul_with_arithmetic_inner_dim() {
    // a :: Matrix[M, 2*K, F64], b :: Matrix[2*K, N, F64] should typecheck
    // because both inner dims simplify to (2*K).
    let two_k = ShapeExpr::product(ShapeExpr::lit(2), ShapeExpr::var("K"));
    let stage_src = "fn matmul(a :: Float, b :: Float) -> Float { a * b }\n";
    let stage = make_stage(stage_src, "matmul");
    let cs = CoreStage {
        stage,
        type_params: vec!["M".into(), "K".into(), "N".into()],
        param_types: vec![
            CoreType::Tensor(matrix(ShapeExpr::var("M"), two_k.clone(), "F64")),
            CoreType::Tensor(matrix(two_k, ShapeExpr::var("N"), "F64")),
        ],
        return_type: CoreType::Tensor(matrix(ShapeExpr::var("M"), ShapeExpr::var("N"), "F64")),
    };
    assert!(check_core_stage(cs).is_ok());
}
