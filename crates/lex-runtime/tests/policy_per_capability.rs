//! Acceptance tests for #207: per-capability effect parameterization
//! at the **runtime policy walk** layer.
//!
//! Mirrors the type-system tests in
//! `lex-types/tests/effect_parameterization.rs` but exercises the
//! `--allow-effects` allowlist semantics (grant subsumption, CLI
//! grant-string parsing). The two layers must agree: a program that
//! type-checks under a given grant set must also pass the policy
//! walk with the same grants, and vice versa.

use lex_ast::canonicalize_program;
use lex_bytecode::compile_program;
use lex_runtime::{check_program, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type-check failed:\n{errs:#?}");
    }
    compile_program(&stages)
}

fn policy_with(allows: &[&str]) -> Policy {
    let mut s = BTreeSet::new();
    for a in allows { s.insert((*a).into()); }
    Policy { allow_effects: s, ..Policy::default() }
}

const PROGRAM_USING_MCP_OCPP: &str = r#"
fn callee() -> [mcp("ocpp")] Int { 1 }
fn entry() -> [mcp("ocpp")] Int { callee() }
"#;

const PROGRAM_USING_FS_READ_PATH: &str = r#"
fn callee() -> [fs_read("/etc/lex.conf")] Int { 1 }
fn entry() -> [fs_read("/etc/lex.conf")] Int { callee() }
"#;

#[test]
fn bare_grant_permits_specific_effect_via_colon_form() {
    // `--allow-effects mcp` should permit `[mcp("ocpp")]` since the
    // bare grant is a wildcard.
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&["mcp"]);
    check_program(&prog, &policy).expect("bare grant should subsume specific");
}

#[test]
fn parens_form_grant_permits_matching_specific_effect() {
    // `--allow-effects mcp(ocpp)` should permit `[mcp("ocpp")]`.
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&["mcp(ocpp)"]);
    check_program(&prog, &policy).expect("specific grant should match");
}

#[test]
fn colon_form_grant_permits_matching_specific_effect() {
    // CLI-friendly `--allow-effects mcp:ocpp` form.
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&["mcp:ocpp"]);
    check_program(&prog, &policy).expect("colon-form grant should match");
}

#[test]
fn specific_grant_rejects_other_specific_effect() {
    // `--allow-effects mcp:optimizer` does NOT cover `[mcp("ocpp")]`.
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&["mcp:optimizer"]);
    let violations = check_program(&prog, &policy).unwrap_err();
    assert!(violations.iter().any(|v| v.kind == "effect_not_allowed"),
        "expected effect_not_allowed; got: {violations:#?}");
}

#[test]
fn empty_allowlist_rejects_specific_effect() {
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&[]);
    let violations = check_program(&prog, &policy).unwrap_err();
    assert!(violations.iter().any(|v| v.kind == "effect_not_allowed"),
        "expected effect_not_allowed; got: {violations:#?}");
}

#[test]
fn fs_read_specific_grant_works_via_colon_form() {
    // `--allow-effects fs_read:/etc/lex.conf` permits the file-scoped
    // declaration. The path-allowlist (`--allow-fs-read`) check fires
    // separately and is exercised in the per-path scoping path; here
    // we're pinning the *kind* allowance only.
    let prog = compile(PROGRAM_USING_FS_READ_PATH);
    let policy = Policy {
        allow_effects: {
            let mut s = BTreeSet::new();
            s.insert("fs_read:/etc/lex.conf".into());
            s
        },
        // Match the path so the second check (fs_path_not_allowed)
        // doesn't fire.
        allow_fs_read: vec!["/etc/lex.conf".into()],
        ..Policy::default()
    };
    check_program(&prog, &policy)
        .expect("scoped fs_read grant should match exact path");
}

#[test]
fn violation_message_names_parameterized_effect() {
    // The error text under #207 should identify *which* parameterized
    // effect was rejected, not just the bare kind.
    let prog = compile(PROGRAM_USING_MCP_OCPP);
    let policy = policy_with(&[]);
    let violations = check_program(&prog, &policy).unwrap_err();
    let v = violations.iter().find(|v| v.kind == "effect_not_allowed").unwrap();
    let effect = v.effect.as_deref().unwrap_or("");
    assert!(effect.contains("mcp") && effect.contains("ocpp"),
        "violation should mention the parameterized form; got: {effect:?}");
}
