//! Write-time type-check gate (#130) wired into
//! `Store::publish_program`. Verifies that a publish whose source
//! doesn't typecheck returns `StoreError::TypeError` with the
//! structured envelope and leaves the branch head unchanged.

use lex_ast::canonicalize_program;
use lex_store::{Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_vcs::{DiffReport, ImportMap};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    let prog = parse_source(src).expect("parse");
    canonicalize_program(&prog)
}

#[test]
fn publish_program_rejects_program_with_unknown_identifier() {
    let (store, _tmp) = fresh();

    // `not_defined` is referenced but never declared — type checker
    // emits an `UnknownIdentifier`. publish_program runs the gate
    // before any side-effect, so the rejection must leave the branch
    // head unchanged and persist no ops.
    let stages = parse("fn broken(x :: Int) -> Int { not_defined(x) }\n");
    let diff = DiffReport::default();
    let imports = ImportMap::default();

    let err = store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, /*activate=*/ true)
        .expect_err("expected TypeError");

    match err {
        StoreError::TypeError(errs) => {
            assert!(!errs.is_empty(), "expected at least one TypeError");
        }
        other => panic!("expected StoreError::TypeError, got {other:?}"),
    }

    // Branch head unchanged — store's "always-valid HEAD" invariant.
    assert!(
        store.get_branch(DEFAULT_BRANCH).unwrap().is_none(),
        "branch should not have been created on the rejection path",
    );

    // No op records persisted.
    let ops_dir = store.root().join("ops");
    if ops_dir.exists() {
        let count = std::fs::read_dir(&ops_dir).unwrap().count();
        assert_eq!(count, 0, "no op records should be persisted on TypeError");
    }
}

#[test]
fn publish_program_rejects_program_with_arity_mismatch() {
    // Verifies that multiple `TypeError` variants flow through the
    // gate (not just `UnknownIdentifier`). Calling `add` with one
    // arg when it takes two emits an `ArityMismatch`.
    let (store, _tmp) = fresh();
    let stages = parse(
        "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn caller() -> Int { add(1) }\n",
    );
    let err = store
        .publish_program(
            DEFAULT_BRANCH,
            &stages,
            &DiffReport::default(),
            &ImportMap::default(),
            true,
        )
        .expect_err("expected TypeError");
    assert!(matches!(err, StoreError::TypeError(_)));
}

#[test]
fn publish_program_accepts_clean_program() {
    // Sanity check that the gate doesn't false-positive: a
    // syntactically and type-correct program publishes through.
    // We don't assert on the diff machinery here (that's covered
    // by the parallel #129 work); just that the gate doesn't
    // refuse a typing-clean program.
    let (store, _tmp) = fresh();
    let stages = parse(
        "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
    );

    // For an empty diff/imports, publish_program produces zero
    // ops (nothing to apply against the empty old-side view). The
    // gate runs first regardless, so the test demonstrates that
    // "clean program → no error" — which is the property we care
    // about for the gate's correctness, decoupled from whatever
    // diff_to_ops happens to do with an empty diff.
    let outcome = store
        .publish_program(
            DEFAULT_BRANCH,
            &stages,
            &DiffReport::default(),
            &ImportMap::default(),
            true,
        )
        .expect("clean program should pass the gate");
    // `outcome.ops` may be empty (no diff entries to convert); the
    // load-bearing assertion is that we didn't error out.
    assert!(outcome.ops.is_empty() || !outcome.ops.is_empty());
}
