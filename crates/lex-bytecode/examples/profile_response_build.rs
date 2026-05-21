//! Callgrind-targeted profiling harness for the response_build
//! workload (#461 follow-up). Compiles the same handler the
//! `response_build` bench uses, then runs `drive(n)` in a tight
//! loop with no criterion overhead so callgrind's per-fn
//! instruction counts attribute cleanly to VM dispatch frames.
//!
//! Run under callgrind:
//!   cargo build --release --example profile_response_build -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.out \
//!     target/release/examples/profile_response_build 400 30
//!
//! Args: <n> <iters>. Defaults 400 30.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
type Response = { status :: Int, total :: Int }

fn handle(user_id :: Int, item_id :: Int, qty :: Int) -> Response {
  let v1 := { a: user_id, b: item_id, c: qty }
  let v2 := { d: v1.a, e: v1.b, f: v1.c, g: v1.a * 2 }
  let v3 := { h: v2.d, i: v2.e, j: v2.f, k: v2.g }
  let v4 := { l: v3.h * 3, m: v3.i * 5, n: v3.j * 7, o: v3.k }
  let v5 := { p: v4.l + v4.m, q: v4.n + v4.o, r: v4.l - v4.m }
  let v6 := { s: v5.p + v5.q, t: v5.q + v5.r, u: v5.p - v5.r }
  match v6.s > 0 {
    true  => { status: 200, total: v6.s + v6.t + v6.u },
    false => { status: 400, total: 0 },
  }
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n, 7, 3)
      r.total + drive(n - 1)
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
    let (mut hits, mut misses, mut skips) = (0u64, 0u64, 0u64);
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        if let Value::Int(v) = vm.call("drive", vec![Value::Int(n)]).unwrap() {
            acc = acc.wrapping_add(v);
        }
        hits += vm.pure_memo_hits;
        misses += vm.pure_memo_misses;
        skips += vm.pure_memo_skips;
    }
    eprintln!("pure_memo: hits={hits} misses={misses} skips={skips}");
    // Keep the result observable so the optimizer can't elide the loop.
    std::process::exit((acc & 0x7f) as i32);
}
