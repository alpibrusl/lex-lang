//! End-to-end integration test for #206 slice 1: an agent harness
//! submits canonical-AST bytes, decodes, type-checks, compiles, and
//! runs — all without touching the text parser. Pure encode/decode
//! correctness lives in `lex-ast/tests/canonical_format.rs`.

use lex_ast::canonical_format::{decode_program, encode_program};
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const PROGRAM: &str = r#"
fn add(x :: Int, y :: Int) -> Int { x + y }
fn run() -> Int { add(2, 3) }
"#;

#[test]
fn agent_submits_canonical_ast_and_runs_end_to_end() {
    // 1. Author side: parse text → canonicalize → encode bytes.
    //    This is the single moment the parser runs.
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages);

    // 2. Wire: agent ships `bytes` to the runtime side. No text
    //    parsing happens at the receiver.

    // 3. Receiver side: decode → type-check → compile → run.
    //    Importantly, lex_syntax::parse_source is NOT called.
    let received = decode_program(&bytes).expect("decode");
    if let Err(errs) = lex_types::check_program(&received) {
        panic!("type-check failed on received canonical AST: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&received));
    let handler = DefaultHandler::new(Policy::permissive())
        .with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let v = vm.call("run", vec![]).expect("call run");
    assert_eq!(v, Value::Int(5));
}

#[test]
fn op_ids_are_bit_identical_across_canonical_round_trip() {
    // The acceptance criterion: lex-vcs op / stage IDs are
    // bit-identical when the same logical program is fed back
    // through compile after a canonical-AST round-trip.
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages);
    let decoded = decode_program(&bytes).expect("decode");

    let bc_a = compile_program(&stages);
    let bc_b = compile_program(&decoded);

    // The canonical encoding plus the post-#222 body-hash work
    // means equivalent programs produce identical body hashes.
    assert_eq!(bc_a.functions.len(), bc_b.functions.len());
    for (fa, fb) in bc_a.functions.iter().zip(bc_b.functions.iter()) {
        assert_eq!(fa.name, fb.name);
        assert_eq!(fa.body_hash, fb.body_hash,
            "function `{}` body hash must match across canonical round-trip",
            fa.name);
    }
}

#[test]
fn canonical_bytes_compile_to_same_running_program() {
    // Run the program twice — once via the text parser, once via
    // canonical bytes — and confirm the answer matches. Pins the
    // semantic identity of the two compilation paths.
    let stages_text = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages_text);
    let stages_canonical = decode_program(&bytes).expect("decode");

    let answer_text = run_via(&stages_text);
    let answer_canonical = run_via(&stages_canonical);
    assert_eq!(answer_text, answer_canonical);
    assert_eq!(answer_text, Value::Int(5));
}

fn run_via(stages: &[lex_ast::Stage]) -> Value {
    if let Err(errs) = lex_types::check_program(stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(stages));
    let handler = DefaultHandler::new(Policy::permissive())
        .with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call("run", vec![]).expect("call run")
}
