//! Performance + correctness budget for `compute_diff`'s rename detection
//! (alpibrusl/lex-lang#813).
//!
//! `pkg_publish_handler` calls `compute_diff` once per uploaded file, with
//! `a` sized to the *entire* tenant's historical function set and `b` to
//! just that file's functions. The rename-detection loop used to recompute
//! each `only_b` candidate's body hash on every `only_a` iteration —
//! O(|only_a| * |only_b|) full clone+serialize+SHA-256 calls — which hung a
//! real production publish for 38+ minutes once a tenant accumulated enough
//! history. This guards against that regression at a scale representative
//! of the incident (thousands of old functions, dozens of new ones per
//! file), not full production scale.

use lex_ast::{CExpr, FnDecl, Param, TypeExpr};
use lex_vcs::compute_diff;
use std::collections::BTreeMap;
use std::time::Instant;

fn int_ty() -> TypeExpr {
    TypeExpr::Named { name: "Int".into(), args: Vec::new() }
}

/// A trivial `fn <name>(n :: Int) -> Int { n + <lit> }`. `lit` controls the
/// body's structural identity independent of the name, so two decls with
/// the same `lit` hash equal under `compute_diff`'s rename detection.
fn fn_decl(name: &str, lit: i64) -> FnDecl {
    FnDecl {
        name: name.into(),
        type_params: Vec::new(),
        params: vec![Param { name: "n".into(), ty: int_ty() }],
        effects: Vec::new(),
        effect_row_var: None,
        return_type: int_ty(),
        body: CExpr::BinOp {
            op: "+".into(),
            lhs: Box::new(CExpr::Var { name: "n".into() }),
            rhs: Box::new(CExpr::Literal { value: lex_ast::CLit::Int { value: lit } }),
        },
        examples: Vec::new(),
    }
}

const OLD_HISTORY_SIZE: usize = 15000;
const NEW_FILE_SIZE: usize = 40;

#[test]
fn rename_detection_scales_linearly_not_quadratically() {
    // Literal bases kept far apart (and independent of OLD_HISTORY_SIZE) so
    // old/new bodies never accidentally collide on hash regardless of scale.
    const OLD_LIT_BASE: i64 = 0;
    const NEW_LIT_BASE: i64 = 1_000_000_000;

    // `a`: a large, unrelated "tenant history" — none of these names or
    // bodies appear in `b`, so every one lands in `only_a`.
    let a: BTreeMap<String, FnDecl> = (0..OLD_HISTORY_SIZE)
        .map(|i| (format!("old_fn_{i}"), fn_decl(&format!("old_fn_{i}"), OLD_LIT_BASE + i as i64)))
        .collect();

    // `b`: one file's worth of functions, all new relative to `a` (so they
    // land in `only_b` and drive the nested rename-detection loop), except
    // one deliberate rename (same body hash as an `a` entry, new name) to
    // prove correctness survives the rewrite.
    let mut b: BTreeMap<String, FnDecl> = (0..NEW_FILE_SIZE)
        .map(|i| (format!("new_fn_{i}"), fn_decl(&format!("new_fn_{i}"), NEW_LIT_BASE + i as i64)))
        .collect();
    b.insert("renamed_old_fn_7".into(), fn_decl("renamed_old_fn_7", OLD_LIT_BASE + 7));

    let start = Instant::now();
    let report = compute_diff(&a, &b, false);
    let elapsed = start.elapsed();

    // Correctness: the deliberate rename is detected, and it's excluded
    // from both removed/added.
    assert_eq!(report.renamed.len(), 1, "expected exactly one detected rename");
    assert_eq!(report.renamed[0].from, "old_fn_7");
    assert_eq!(report.renamed[0].to, "renamed_old_fn_7");
    assert!(!report.removed.iter().any(|r| r.name == "old_fn_7"));
    assert!(!report.added.iter().any(|a| a.name == "renamed_old_fn_7"));
    // `b` has NEW_FILE_SIZE original entries plus the one inserted rename
    // target; one of the NEW_FILE_SIZE + 1 total is consumed by the rename,
    // leaving NEW_FILE_SIZE as added. `a` has OLD_HISTORY_SIZE entries;
    // one is consumed by the rename, leaving OLD_HISTORY_SIZE - 1 removed.
    assert_eq!(report.removed.len(), OLD_HISTORY_SIZE - 1);
    assert_eq!(report.added.len(), NEW_FILE_SIZE);

    // Performance: O(|only_a| * |only_b|) unmemoized hashing at this scale
    // (3000 * 40 = 120,000 clone+serialize+SHA-256 calls) is what hung the
    // real server; the fixed O(|only_a| + |only_b|) version should be near
    // instant. Budget is generous to stay stable on slow CI runners.
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "compute_diff({OLD_HISTORY_SIZE} old, {NEW_FILE_SIZE} new) took {elapsed:?}; \
         a quadratic regression in rename detection would blow this budget (see #813)"
    );
}
