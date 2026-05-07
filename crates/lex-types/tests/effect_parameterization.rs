//! Acceptance tests for #207: per-capability effect parameterization.
//!
//! Subsumption rules pinned here:
//!   - bare `[name]` absorbs any `[name(...)]` (wildcard semantics).
//!   - specific `[name(arg)]` matches only itself; *not* other args.
//!   - specific does not grant bare.
//!   - distinct names never match.
//!
//! At the syntax level the parser has accepted `[name("arg")]` for a
//! while; the type checker now actually honors the arg.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{check_program, TypeError};

fn check(src: &str) -> Result<(), Vec<TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

// --- happy paths ---

#[test]
fn bare_caller_satisfies_specific_callee() {
    // Caller declares [mcp]; callee requires [mcp("ocpp")].
    // Wildcard absorbs specific, so the call typechecks.
    let src = r#"
fn callee() -> [mcp("ocpp")] Int { 1 }
fn caller() -> [mcp] Int { callee() }
"#;
    check(src).unwrap_or_else(|errs| panic!("expected ok, got: {errs:#?}"));
}

#[test]
fn specific_caller_satisfies_same_specific_callee() {
    // Both declare [mcp("ocpp")] — exact match.
    let src = r#"
fn callee() -> [mcp("ocpp")] Int { 1 }
fn caller() -> [mcp("ocpp")] Int { callee() }
"#;
    check(src).unwrap_or_else(|errs| panic!("expected ok, got: {errs:#?}"));
}

// --- error paths ---

#[test]
fn specific_caller_does_not_grant_other_specific() {
    // Caller declares [mcp("optimizer")]; callee requires [mcp("ocpp")].
    // Different args → caller is *not* permissive enough for callee.
    let src = r#"
fn callee() -> [mcp("ocpp")] Int { 1 }
fn caller() -> [mcp("optimizer")] Int { callee() }
"#;
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::EffectNotDeclared { .. })),
        "expected EffectNotDeclared, got: {errs:#?}");
}

#[test]
fn specific_caller_does_not_grant_bare_callee() {
    // Caller declares [net("wttr.in")]; callee requires [net].
    // The bare form means "any host", which the specific can't grant —
    // there might be a different host the callee uses.
    let src = r#"
fn callee() -> [net] Int { 1 }
fn caller() -> [net("wttr.in")] Int { callee() }
"#;
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::EffectNotDeclared { .. })),
        "expected EffectNotDeclared, got: {errs:#?}");
}

#[test]
fn empty_caller_does_not_grant_specific() {
    let src = r#"
fn callee() -> [fs_read("/etc")] Int { 1 }
fn caller() -> Int { callee() }
"#;
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::EffectNotDeclared { .. })),
        "expected EffectNotDeclared, got: {errs:#?}");
}

// --- error rendering ---

#[test]
fn parameterized_effect_appears_in_error_message() {
    // Confirms that the renderer uses `EffectKind::pretty` so the
    // mismatch message identifies the parameterized form rather
    // than just the bare name.
    let src = r#"
fn callee() -> [fs_read("/etc/passwd")] Int { 1 }
fn caller() -> Int { callee() }
"#;
    let errs = check(src).unwrap_err();
    // Walk the structured error so we don't have to second-guess
    // Debug's quoting around the inner string arg.
    let msg = match errs.first() {
        Some(TypeError::EffectNotDeclared { effect, .. }) => effect.clone(),
        other => panic!("expected EffectNotDeclared, got: {other:#?}"),
    };
    assert!(msg.contains("fs_read") && msg.contains("/etc/passwd"),
        "error should name the parameterized effect; got: {msg}");
}
