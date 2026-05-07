//! Effect-set soundness for dead-branch elimination (#228).
//!
//! When a `[net]`-using path lives in a constant-`false` arm, the
//! function's *inferred* effect set must drop `[net]` so the
//! type-checker (and the runtime policy walk that follows) sees
//! only the live code's effects. Without the dead-branch pass
//! running before type-check, the function would type-check as
//! `[io, net]` and a policy without `--allow-effects net` would
//! incorrectly reject it.
//!
//! Lives in `lex-runtime` rather than `lex-ast` because exercising
//! the type-checker requires `lex-types`, which depends on `lex-ast`
//! and would form a circular dev-dep if pulled in there.

use lex_ast::canonicalize_program;
use lex_runtime::{check_program, Policy};
use lex_syntax::parse_source;
use lex_bytecode::compile_program;
use std::collections::BTreeSet;

fn compile_under(src: &str, allows: &[&str]) -> Result<(), String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return Err(format!("type errors: {errs:#?}"));
    }
    let bc = compile_program(&stages);
    let mut s = BTreeSet::new();
    for a in allows { s.insert((*a).into()); }
    let policy = Policy { allow_effects: s, ..Policy::default() };
    check_program(&bc, &policy).map(|_| ())
        .map_err(|v| format!("policy violations: {v:#?}"))
}

#[test]
fn dead_net_call_does_not_force_caller_to_declare_net() {
    // `if true { ... } else { net.get(...) }` should drop the
    // `[net]` effect after dead-branch elimination, so a function
    // declaring only `[io]` type-checks even though the source
    // mentions `net.get`.
    //
    // The inferred set for the function body matters: if the
    // dead-branch pass did NOT run before type-check, the
    // type-checker would walk both arms, observe the [net] call
    // in the dead arm, and conclude the function's body uses
    // [net] — which would conflict with the `[io]` declaration.
    let src = r#"
import "std.net" as net
fn fetch_or_default() -> [io] Str {
  if true { "" } else { net.get("https://example.com") }
}
"#;
    compile_under(src, &["io"])
        .expect("dead-branch pass should drop [net] from inferred set");
}

#[test]
fn live_net_call_still_requires_net_declaration() {
    // Sanity: with the predicate flipped, the [net] arm IS live
    // and the type-checker rejects an `[io]`-only declaration.
    let src = r#"
import "std.net" as net
fn fetch_or_default() -> [io] Str {
  if false { "" } else { net.get("https://example.com") }
}
"#;
    let err = compile_under(src, &["io"])
        .expect_err("live [net] call should still trip type-check");
    // "type errors" comes from check_program's wrapping; either
    // type-check or policy walk error is acceptable as long as
    // the program is rejected.
    assert!(err.contains("type errors") || err.contains("policy violations"),
        "expected rejection with type or policy error; got: {err}");
}

#[test]
fn non_constant_predicate_keeps_net_in_inferred_set() {
    // When the predicate isn't a literal, the dead-branch pass
    // doesn't fire and BOTH arms contribute to the inferred set.
    // An `[io]`-only declaration should fail.
    let src = r#"
import "std.net" as net
fn fetch_or_default(b :: Bool) -> [io] Str {
  if b { "" } else { net.get("https://example.com") }
}
"#;
    let err = compile_under(src, &["io"])
        .expect_err("dynamic predicate should preserve [net] in inferred set");
    assert!(err.contains("type errors") || err.contains("policy violations"),
        "expected rejection; got: {err}");
}
