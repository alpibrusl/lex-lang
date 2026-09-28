//! `todo()`: a checker-recognized placeholder call, not a real global
//! (docs/design/project-to-issue-graph.md §3). It type-checks as
//! `Never` — the same bottom type already used for `return`'s
//! expression position — so it unifies with whatever the surrounding
//! context expects, and exists only in immediate-call position.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{check_program, TypeError};

fn check(src: &str) -> Result<(), Vec<TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

fn assert_ok(src: &str) {
    check(src).unwrap_or_else(|errs| panic!("expected ok; got: {errs:#?}"));
}

#[test]
fn placeholder_body_unifies_with_int_return() {
    assert_ok("fn f() -> Int { todo() }\n");
}

#[test]
fn placeholder_body_unifies_with_str_return() {
    assert_ok("fn f() -> Str { todo() }\n");
}

#[test]
fn placeholder_body_unifies_with_user_type_return() {
    assert_ok(r#"
type Shape = Circle(Int) | Square(Int)

fn f() -> Shape { todo() }
"#);
}

#[test]
fn placeholder_arg_unifies_with_declared_param_type() {
    // A stub whose callee's signature is already fixed can still use
    // `todo()` for one of its own arguments — the whole point of §4's
    // skeleton-compile step is that every stub in the graph, not just
    // leaf functions, is real Lex source that typechecks together.
    assert_ok(r#"
fn callee(x :: Int) -> Int { x }
fn f() -> Int { callee(todo()) }
"#);
}

#[test]
fn wrong_arity_is_rejected() {
    let src = "fn f() -> Int { todo(1) }\n";
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::ArityMismatch { expected: 0, got: 1, .. })),
        "expected an ArityMismatch(0, 1) among: {errs:#?}"
    );
}

#[test]
fn bare_uncalled_reference_is_an_unknown_identifier() {
    // `todo` is recognized only as an immediate call; a bare reference
    // (passed as a value, not applied) is an ordinary unknown
    // identifier, not a value that could panic at runtime if it's
    // ever actually invoked through some other path.
    let src = "fn f() -> Int { todo }\n";
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::UnknownIdentifier { name, .. } if name == "todo")),
        "expected an UnknownIdentifier(\"todo\") among: {errs:#?}"
    );
}

#[test]
fn user_defined_todo_shadows_the_placeholder() {
    // A user's own `fn todo(...)` takes precedence over the built-in
    // placeholder, same as any other name — the placeholder call form
    // only kicks in when the program doesn't declare its own `todo`.
    assert_ok(r#"
fn todo() -> Int { 42 }
fn f() -> Int { todo() }
"#);
}

#[test]
fn user_defined_todo_with_different_arity_is_not_shadowed_by_the_placeholder() {
    // Once the user declares their own `todo`, its real signature
    // governs — calling it with the placeholder's arity (zero args)
    // is a normal arity error against the user's declaration, not a
    // silent fall-through to the placeholder.
    let src = r#"
fn todo(reason :: Str) -> Int { 0 }
fn f() -> Int { todo() }
"#;
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::ArityMismatch { expected: 1, got: 0, .. })),
        "expected an ArityMismatch(1, 0) among: {errs:#?}"
    );
}
