//! Callgrind harness for a tail-recursive loop workload (#464
//! call-overhead). An accumulator countdown that the compiler lowers
//! to `Op::TailCall` (tail position in a `match` arm, see
//! compiler.rs:207 + compile_match): the recursive call reuses the
//! current frame instead of pushing a new one, so deep recursion runs
//! in constant stack. This is the idiomatic shape for loops in Lex, and
//! the path that — before #464 — allocated a fresh args Vec per tail
//! call. Both `drive -> sum_to` and `sum_to -> sum_to` are tail calls,
//! so the run is dominated by `Op::TailCall`.
//!
//!   cargo build --release --example profile_tailrec -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.tail \
//!     target/release/examples/profile_tailrec 20000 3
//!
//! Args: <n> <iters>. Defaults 20000 3.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
fn sum_to(n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_to(n - 1, acc + n),
  }
}

fn drive(n :: Int) -> Int {
  sum_to(n, 0)
}
"#;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(20000);
    let iters: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);

    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));

    let mut acc = 0i64;
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        if let Value::Int(v) = vm.call("drive", vec![Value::Int(n)]).unwrap() {
            acc = acc.wrapping_add(v);
        }
    }
    std::process::exit((acc & 0x7f) as i32);
}
