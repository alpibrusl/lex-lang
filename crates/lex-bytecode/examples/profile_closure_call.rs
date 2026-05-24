//! Callgrind harness for a closure-call workload (#464 call-overhead).
//! A function-typed parameter `f` is invoked directly — `f(n)` where
//! `f` is a local — which the compiler lowers to `Op::CallClosure`
//! (compiler.rs:1088), the path that allocates a per-call args Vec and
//! concatenates `captures ++ args`. The closure `g` captures `base`, so
//! the captures slice is non-empty and the captures-into-locals path is
//! exercised too. `list.map`/`fold` use the native invoke_closure_* fast
//! path instead, so a directly-called function-typed param is the way to
//! drive Op::CallClosure.
//!
//!   cargo build --release --example profile_closure_call -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.cc \
//!     target/release/examples/profile_closure_call 20000 3
//!
//! Args: <n> <iters>. Defaults 20000 3.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
fn sum_apply(f :: (Int) -> Int, n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_apply(f, n - 1, acc + f(n) + f(n + 1)),
  }
}

fn drive(n :: Int) -> Int {
  let base := n + 7
  let g := fn(x :: Int) -> Int { x * 2 + base }
  sum_apply(g, n, 0)
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
