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
    // #463 slice 2b-i measurement: when set, wrap each iter's
    // `vm.call("drive")` in an enter/exit_request_scope pair and
    // materialize the result — mirrors the `net.serve_fn` flow so
    // arena-lowered ops actually fire. Without the flag, the call
    // runs outside any scope, the alloc ops fall back to MakeRecord,
    // and the profile captures the pre-arena baseline shape.
    let arena_scope = std::env::var_os("LEX_PROFILE_ARENA").is_some();

    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));

    // Diagnostic: how many AllocArenaRecord / AllocStackRecord /
    // MakeRecord sites in `handle()`? Lets the measurement reader
    // verify whether the arena lowering pass actually fired before
    // attributing the I-refs to it.
    {
        use lex_bytecode::Op;
        let h = &p.functions[p.function_names["handle"] as usize];
        let arena = h.code.iter().filter(|o| matches!(o, Op::AllocArenaRecord { .. })).count();
        let stack = h.code.iter().filter(|o| matches!(o, Op::AllocStackRecord { .. })).count();
        let heap = h.code.iter().filter(|o| matches!(o, Op::MakeRecord { .. })).count();
        eprintln!("[diag] handle() record sites: arena={arena}, stack={stack}, heap={heap}");
    }

    let mut acc = 0i64;
    let mut tot_arena = 0u64;
    let mut tot_arena_fb = 0u64;
    let mut tot_stack = 0u64;
    let mut tot_heap = 0u64;
    let (mut hits, mut misses, mut skips) = (0u64, 0u64, 0u64);
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        let r = if arena_scope {
            let scope = vm.enter_request_scope();
            let r = vm.call("drive", vec![Value::Int(n)]);
            // Materialize before exit so the result is heap-owned
            // and the slab can drop safely — matches the runtime's
            // `materialize_response_body` discipline.
            let r = r.map(|v| vm.materialize_arena_handles(v));
            vm.exit_request_scope(scope);
            r
        } else {
            vm.call("drive", vec![Value::Int(n)])
        };
        if let Value::Int(v) = r.unwrap() {
            acc = acc.wrapping_add(v);
        }
        hits += vm.pure_memo_hits;
        misses += vm.pure_memo_misses;
        skips += vm.pure_memo_skips;
        tot_arena += vm.arena_record_allocs;
        tot_arena_fb += vm.arena_record_heap_fallbacks;
        tot_stack += vm.stack_record_allocs;
        tot_heap += vm.heap_record_allocs;
    }
    eprintln!("pure_memo: hits={hits} misses={misses} skips={skips}");
    eprintln!("[diag] arena_scope_mode={arena_scope}");
    eprintln!("[diag] runtime counters: arena_allocs={tot_arena}, arena_fallbacks={tot_arena_fb}, stack_allocs={tot_stack}, heap_allocs={tot_heap}");
    // Keep the result observable so the optimizer can't elide the loop.
    std::process::exit((acc & 0x7f) as i32);
}
