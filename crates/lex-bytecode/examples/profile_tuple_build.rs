//! Callgrind-targeted profiling harness for a tuple-heavy workload
//! (#464 tuple codegen measurement). Mirrors `profile_response_build`
//! but exercises non-escaping *tuples* (built and destructured via
//! `match`) rather than records, so callgrind attributes the
//! `AllocStackTuple` stack path vs the `MakeTuple` heap path cleanly.
//!
//! A/B the same source under matched VM/peephole conditions with the
//! `LEX_NO_STACK_RECORDS` escape hatch (it gates the whole escape
//! lowering, tuples included):
//!
//!   cargo build --release --example profile_tuple_build -p lex-bytecode
//!   # stack-tuple path (lowering on):
//!   valgrind --tool=callgrind --callgrind-out-file=cg.on \
//!     target/release/examples/profile_tuple_build 400 30
//!   # heap-tuple baseline (lowering off):
//!   LEX_NO_STACK_RECORDS=1 valgrind --tool=callgrind \
//!     --callgrind-out-file=cg.off \
//!     target/release/examples/profile_tuple_build 400 30
//!
//! Args: <n> <iters>. Defaults 400 30.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
fn handle(a :: Int, b :: Int) -> Int {
  let s1 := match (a, b)   { (x, y) => x + y }
  let s2 := match (a, b)   { (x, y) => x * y }
  let s3 := match (s1, s2) { (x, y) => x + y }
  let s4 := match (s1, s2) { (x, y) => x - y }
  let s5 := match (s3, s4) { (x, y) => x * 2 + y }
  let s6 := match (s4, s3) { (x, y) => x + y * 3 }
  s1 + s2 + s3 + s4 + s5 + s6
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n, 7)
      r + drive(n - 1)
    },
  }
}
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(400);
    let iters: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);

    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));

    let mut acc = 0i64;
    let (mut stack, mut fallback) = (0u64, 0u64);
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        if let Value::Int(v) = vm.call("drive", vec![Value::Int(n)]).unwrap() {
            acc = acc.wrapping_add(v);
        }
        stack += vm.stack_record_allocs;
        fallback += vm.stack_record_heap_fallbacks;
    }
    eprintln!("stack_tuples: stack={stack} fallback={fallback}");
    // Keep the result observable so the optimizer can't elide the loop.
    std::process::exit((acc & 0x7f) as i32);
}
