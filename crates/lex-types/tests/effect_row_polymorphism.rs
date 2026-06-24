//! Acceptance tests for effect-row polymorphism: open effect rows on
//! user-defined functions via the `[base | E]` surface syntax.
//!
//! A row variable `E` (declared as a type parameter) stands for "plus any
//! further effects". It generalizes at the definition and is bound per call
//! site to the caller's actual effects — the same mechanism the stdlib HOFs
//! (`list.map` etc.) already use, now available to user code, and the piece
//! that lets a generic server (`run_http[E]`) forward an actuating handler's
//! effects without the framework hard-coding `sense`/`actuate`.
//!
//! The load-bearing safety property: open rows *propagate and enforce* the
//! extra effects (no silent drop), while *closed* rows keep their strict
//! equality checking unchanged (the effect wall).

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{check_program, TypeError};

fn check(src: &str) -> Result<(), Vec<TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

fn err_kinds(errs: &[TypeError]) -> Vec<String> {
    errs.iter().map(|e| format!("{e:?}")).collect()
}

// --- row-polymorphic parameters -------------------------------------------

#[test]
fn row_poly_param_propagates_caller_effects() {
    // `run` is polymorphic over the callback's extra effects E. Passing a
    // callback that uses `time` makes the caller require `io, time`.
    let src = r#"
import "std.time" as time
fn run[E](f :: (Int) -> [io | E] Int, x :: Int) -> [io | E] Int { f(x) }
fn with_time(x :: Int) -> [io, time] Int { let _ := time.now_ms() x }
fn main() -> [io, time] Int { run(with_time, 5) }
"#;
    check(src).unwrap_or_else(|errs| panic!("expected ok, got: {errs:#?}"));
}

#[test]
fn row_poly_param_under_declared_caller_is_rejected() {
    // `main` omits `time`, but `run` propagates it from the callback — the
    // open row must *enforce* the extra effect, not drop it.
    let src = r#"
import "std.time" as time
fn run[E](f :: (Int) -> [io | E] Int, x :: Int) -> [io | E] Int { f(x) }
fn with_time(x :: Int) -> [io, time] Int { let _ := time.now_ms() x }
fn main() -> [io] Int { run(with_time, 5) }
"#;
    let errs = check(src).expect_err("expected effect_not_declared");
    assert!(
        err_kinds(&errs).iter().any(|s| s.contains("EffectNotDeclared")),
        "expected EffectNotDeclared, got: {:#?}",
        errs
    );
}

#[test]
fn row_poly_param_with_pure_callback_requires_nothing_extra() {
    // E binds to the empty row when the callback is closed at the base.
    let src = r#"
fn run[E](f :: (Int) -> [io | E] Int, x :: Int) -> [io | E] Int { f(x) }
fn ioonly(x :: Int) -> [io] Int { x }
fn main() -> [io] Int { run(ioonly, 5) }
"#;
    check(src).unwrap_or_else(|errs| panic!("expected ok, got: {errs:#?}"));
}

// --- the wall: closed rows are unchanged ----------------------------------

#[test]
fn closed_row_param_still_rejects_extra_effect_arg() {
    // A *closed* `[io]` parameter must still reject a `[io, time]` argument —
    // effect rows unify by equality for closed rows. (Regression guard.)
    let src = r#"
import "std.time" as time
fn run_closed(f :: (Int) -> [io] Int, x :: Int) -> [io] Int { f(x) }
fn with_time(x :: Int) -> [io, time] Int { let _ := time.now_ms() x }
fn main() -> [io, time] Int { run_closed(with_time, 5) }
"#;
    let errs = check(src).expect_err("expected effect_row_mismatch");
    assert!(
        err_kinds(&errs).iter().any(|s| s.contains("EffectRowMismatch")),
        "expected EffectRowMismatch, got: {:#?}",
        errs
    );
}

#[test]
fn closed_row_fn_still_rejects_undeclared_body_effect() {
    // The compile-time effect wall for ordinary closed-row functions is
    // untouched: a `[io]` function that calls a `[time]` builtin fails.
    let src = r#"
import "std.time" as time
fn sneaky() -> [io] Int { let _ := time.now_ms() 1 }
"#;
    let errs = check(src).expect_err("expected effect_not_declared");
    assert!(
        err_kinds(&errs).iter().any(|s| s.contains("EffectNotDeclared")),
        "expected EffectNotDeclared, got: {:#?}",
        errs
    );
}

// --- row-polymorphic closures composing through net.serve_fn --------------

#[test]
fn row_poly_closure_forwards_effects_through_serve_fn() {
    // The `run_http` pattern: a generic server forwards a handler's extra
    // effects (here `actuate`) through a row-poly closure into the already
    // row-polymorphic `net.serve_fn`, out to `main`.
    let src = r#"
import "std.net" as net
import "std.map" as map
fn run_http[E](port :: Int, dispatch :: (Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Response) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Nil {
  let handler := fn (req :: Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Response { dispatch(req) }
  net.serve_fn(port, handler)
}
fn robot_dispatch(req :: Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc, actuate] Response {
  let _ := req
  { status: 200, body: BodyStr("ok"), headers: map.from_list([]) }
}
fn main() -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc, actuate] Nil { run_http(8080, robot_dispatch) }
"#;
    check(src).unwrap_or_else(|errs| panic!("expected ok, got: {errs:#?}"));
}

#[test]
fn row_poly_closure_under_declared_outer_is_rejected() {
    // If `main` omits `actuate`, the effect propagated through the closure
    // must still be enforced.
    let src = r#"
import "std.net" as net
import "std.map" as map
fn run_http[E](port :: Int, dispatch :: (Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Response) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Nil {
  let handler := fn (req :: Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Response { dispatch(req) }
  net.serve_fn(port, handler)
}
fn robot_dispatch(req :: Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc, actuate] Response {
  let _ := req
  { status: 200, body: BodyStr("ok"), headers: map.from_list([]) }
}
fn main() -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc] Nil { run_http(8080, robot_dispatch) }
"#;
    let errs = check(src).expect_err("expected effect_not_declared");
    assert!(
        err_kinds(&errs).iter().any(|s| s.contains("EffectNotDeclared")),
        "expected EffectNotDeclared, got: {:#?}",
        errs
    );
}

#[test]
fn closure_dropping_open_row_is_rejected() {
    // A closure whose body produces an open row (by calling a row-poly
    // parameter) but whose *declared* row is closed must be rejected — the
    // extra effects would otherwise be silently dropped at the boundary.
    let src = r#"
import "std.net" as net
import "std.map" as map
fn run_http[E](port :: Int, dispatch :: (Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Response) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc | E] Nil {
  let handler := fn (req :: Request) -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc] Response { dispatch(req) }
  net.serve_fn(port, handler)
}
fn main() -> [io, net, sql, concurrent, random, fs_read, fs_write, time, crypto, llm, proc] Nil { let _ := run_http 1 }
"#;
    let errs = check(src).expect_err("expected effect_not_declared (open row dropped)");
    assert!(
        err_kinds(&errs).iter().any(|s| s.contains("EffectNotDeclared")),
        "expected EffectNotDeclared, got: {:#?}",
        errs
    );
}
