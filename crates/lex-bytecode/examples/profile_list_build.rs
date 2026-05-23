//! Callgrind harness for a list-builder workload (#464 native
//! list ops). Chains `list.map`/`fold` over small lists — the shape
//! that, with the old inlined loops, re-`LoadLocal`'d (cloned) the
//! whole input and accumulator lists each iteration (O(n²)). With the
//! native `Op::ListMap`/`ListFold` ops the VM owns the list and builds
//! results with one pre-sized allocation.
//!
//!   cargo build --release --example profile_list_build -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.list \
//!     target/release/examples/profile_list_build 120 3
//!
//! Args: <n> <iters>. Defaults 400 30.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
import "std.list" as list

fn handle(n :: Int) -> Int {
  let a := [n, n + 1, n + 2, n + 3]
  let b := list.map(a, fn(x :: Int) -> Int { x + 1 })
  let c := list.map(b, fn(x :: Int) -> Int { x * 2 })
  let d := list.map(c, fn(x :: Int) -> Int { x - 1 })
  list.fold(d, 0, fn(acc :: Int, x :: Int) -> Int { acc + x })
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n)
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
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        if let Value::Int(v) = vm.call("drive", vec![Value::Int(n)]).unwrap() {
            acc = acc.wrapping_add(v);
        }
    }
    std::process::exit((acc & 0x7f) as i32);
}
