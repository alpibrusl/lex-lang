//! Conformance for `Stream[T]` + `agent.cloud_stream` (#305 slice 3).
//!
//! Asserts:
//! - The `Stream[Str]` type parses and type-checks.
//! - `agent.cloud_stream` populates a stream from the
//!   `LEX_LLM_STREAM_FIXTURE` env var.
//! - `stream.next` yields chunks lazily, one at a time, in order
//!   (the load-bearing "token-by-token before full response"
//!   property: each chunk is observable individually before the
//!   next is consumed).
//! - `stream.collect` drains the rest to a List[Str].
//! - The producer chain is `Result[Stream[Str], Str]` so transport
//!   errors surface synchronously at handshake.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Every test in this file mutates `LEX_LLM_STREAM_FIXTURE`, which
/// is process-global. Hold this mutex for the duration of each
/// test so cargo's default parallel test runner doesn't race two
/// fixture writers.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

fn build(src: &str) -> lex_bytecode::Program {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let mut policy = Policy::pure();
    policy.allow_effects = ["llm_cloud", "stream"].into_iter().map(String::from).collect::<BTreeSet<_>>();
    check_program(&bc, &policy).expect("program type-checks");
    bc
}

fn run(bc: &lex_bytecode::Program, entry: &str, args: Vec<Value>) -> Value {
    let mut policy = Policy::pure();
    policy.allow_effects = ["llm_cloud", "stream"].into_iter().map(String::from).collect::<BTreeSet<_>>();
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.call(entry, args).unwrap()
}

#[test]
fn stream_collect_yields_all_chunks_in_order() {
    let src = r#"
import "std.agent" as agent
import "std.stream" as stream
fn drain(prompt :: Str) -> [llm_cloud, stream] Result[List[Str], Str] {
    match agent.cloud_stream(prompt) {
        Ok(s) => Ok(stream.collect(s)),
        Err(e) => Err(e),
    }
}
"#;
    let _lock = env_lock();
    let bc = build(src);
    std::env::set_var("LEX_LLM_STREAM_FIXTURE", "alpha|beta|gamma|delta");
    let r = run(&bc, "drain", vec![Value::Str("ignored".into())]);
    std::env::remove_var("LEX_LLM_STREAM_FIXTURE");

    // Result[List[Str], Str] -> Ok([alpha, beta, gamma, delta]).
    match r {
        Value::Variant { name, args } => {
            assert_eq!(name, "Ok", "expected Ok variant: {args:?}");
            let inner = args.into_iter().next().expect("Ok payload");
            assert_eq!(
                inner,
                Value::List(vec![
                    Value::Str("alpha".into()),
                    Value::Str("beta".into()),
                    Value::Str("gamma".into()),
                    Value::Str("delta".into()),
                ]),
            );
        }
        other => panic!("expected Variant, got {other:?}"),
    }
}

#[test]
fn stream_next_is_lazy_one_chunk_at_a_time() {
    // The load-bearing AC: a chunk must be observable individually
    // before its successor is consumed. We pull 2 of 4 chunks via
    // stream.next, drain the rest with stream.collect, and assert
    // the chunks split at the expected boundary.
    //
    // The pull chain runs in a helper so the `let`s are at function-
    // body top level (which has well-supported `let` chains) instead
    // of inside a match-arm block. A record literal would be
    // canonicalized into alphabetical field order, which would
    // change the effect schedule — use a tuple instead.
    let src = r#"
import "std.agent" as agent
import "std.stream" as stream

fn pull_three(s :: Stream[Str]) -> [stream] (Option[Str], Option[Str], List[Str]) {
    let first := stream.next(s)
    let second := stream.next(s)
    let rest := stream.collect(s)
    (first, second, rest)
}

fn pull2(prompt :: Str) -> [llm_cloud, stream] Result[(Option[Str], Option[Str], List[Str]), Str] {
    match agent.cloud_stream(prompt) {
        Ok(s) => Ok(pull_three(s)),
        Err(e) => Err(e),
    }
}
"#;
    let _lock = env_lock();
    let bc = build(src);
    std::env::set_var("LEX_LLM_STREAM_FIXTURE", "one|two|three|four");
    let r = run(&bc, "pull2", vec![Value::Str("ignored".into())]);
    std::env::remove_var("LEX_LLM_STREAM_FIXTURE");

    let Value::Variant { name, args } = r else {
        panic!("expected Variant");
    };
    assert_eq!(name, "Ok");
    let payload = args.into_iter().next().expect("Ok payload");
    let Value::Tuple(items) = payload else {
        panic!("expected Tuple payload");
    };
    let some = |s: &str| Value::Variant {
        name: "Some".into(),
        args: vec![Value::Str(s.into())],
    };
    assert_eq!(items.first().cloned(), Some(some("one")));
    assert_eq!(items.get(1).cloned(), Some(some("two")));
    assert_eq!(
        items.get(2).cloned(),
        Some(Value::List(vec![
            Value::Str("three".into()),
            Value::Str("four".into()),
        ])),
    );
}

#[test]
fn stream_next_returns_none_past_end() {
    // Pulling one more time than the producer has chunks must
    // return None — matches Iterator::next semantics. We exhaust
    // a 2-chunk stream with 3 stream.next calls and assert the
    // third is None. Using `let` chain so the three pulls
    // observably happen in source order.
    let src = r#"
import "std.agent" as agent
import "std.stream" as stream

fn three_pulls(s :: Stream[Str]) -> [stream] (Option[Str], Option[Str], Option[Str]) {
    let a := stream.next(s)
    let b := stream.next(s)
    let c := stream.next(s)
    (a, b, c)
}

fn pull3(prompt :: Str) -> [llm_cloud, stream] Result[(Option[Str], Option[Str], Option[Str]), Str] {
    match agent.cloud_stream(prompt) {
        Ok(s) => Ok(three_pulls(s)),
        Err(e) => Err(e),
    }
}
"#;
    let _lock = env_lock();
    let bc = build(src);
    std::env::set_var("LEX_LLM_STREAM_FIXTURE", "x|y");
    let r = run(&bc, "pull3", vec![Value::Str("p".into())]);
    std::env::remove_var("LEX_LLM_STREAM_FIXTURE");

    let Value::Variant { name: top, args } = r else { panic!() };
    assert_eq!(top, "Ok");
    let Value::Tuple(items) = args.into_iter().next().unwrap() else {
        panic!()
    };
    let some = |s: &str| Value::Variant {
        name: "Some".into(),
        args: vec![Value::Str(s.into())],
    };
    let none = Value::Variant { name: "None".into(), args: vec![] };
    assert_eq!(items.first().cloned(), Some(some("x")));
    assert_eq!(items.get(1).cloned(), Some(some("y")));
    assert_eq!(
        items.get(2).cloned(),
        Some(none),
        "third pull past end must be None"
    );
}

#[test]
fn cloud_stream_without_fixture_returns_err() {
    // Without LEX_LLM_STREAM_FIXTURE the producer surfaces an Err
    // at handshake (Result[Stream[Str], Str]). Validates the
    // synchronous-error path agents need to gate retries on.
    let src = r#"
import "std.agent" as agent
fn handshake(prompt :: Str) -> [llm_cloud] Result[Str, Str] {
    match agent.cloud_stream(prompt) {
        Ok(_) => Ok("got_stream"),
        Err(e) => Err(e),
    }
}
"#;
    let _lock = env_lock();
    let bc = build(src);
    std::env::remove_var("LEX_LLM_STREAM_FIXTURE");
    let r = run(&bc, "handshake", vec![Value::Str("p".into())]);
    let Value::Variant { name, args } = r else { panic!() };
    assert_eq!(name, "Err", "no fixture must produce Err: {args:?}");
    let Value::Str(msg) = args.into_iter().next().unwrap() else { panic!() };
    assert!(
        msg.contains("LEX_LLM_STREAM_FIXTURE"),
        "Err message should mention the fixture env var so test setup is obvious: {msg}"
    );
}

#[test]
fn cloud_stream_without_effect_grant_is_refused() {
    // Calling agent.cloud_stream when [llm_cloud] isn't allowed
    // by policy must fail. Mirrors the existing gate for
    // agent.cloud_complete.
    let src = r#"
import "std.agent" as agent
fn no_grant() -> [llm_cloud] Result[Str, Str] {
    match agent.cloud_stream("p") {
        Ok(_) => Ok("got_stream"),
        Err(e) => Err(e),
    }
}
"#;
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let policy = Policy::pure(); // no allow_effects
    // The policy check refuses the program before run-time.
    let err = check_program(&bc, &policy).expect_err("must reject");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("llm_cloud"),
        "expected llm_cloud refusal, got: {msg}"
    );
}
