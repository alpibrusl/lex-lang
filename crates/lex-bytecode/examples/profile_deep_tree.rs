//! #463 deep-leaf measurement harness.
//!
//! Profile target for the containment-tracking refinement: a
//! handler that returns a 3-deep nested record so every level
//! becomes arena-eligible only under deep-leaf widening. Pre-
//! refinement the inner two levels leaked at the outer's
//! `MakeRecord` build sites; post-refinement the whole tree is
//! a single arena slab allocation.
//!
//! Run under callgrind:
//!   cargo build --release --example profile_deep_tree -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.off.out \
//!     target/release/examples/profile_deep_tree 1000 5
//!   LEX_PROFILE_ARENA=1 valgrind --tool=callgrind \
//!     --callgrind-out-file=cg.on.out \
//!     target/release/examples/profile_deep_tree 1000 5

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Op, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
type Inner  = { a :: Int, b :: Int }
type Middle = { inner :: Inner, c :: Int }
type Outer  = { middle :: Middle, status :: Int }

fn handle(i :: Int) -> Outer {
  { middle: { inner: { a: i, b: i + 1 }, c: i * 2 }, status: 200 }
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n)
      r.status + drive(n - 1)
    },
  }
}
"#;

fn main() {
    let n: i64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
    let iters: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let arena_scope = std::env::var_os("LEX_PROFILE_ARENA").is_some();

    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));

    let h = &p.functions[p.function_names["handle"] as usize];
    let arena = h.code.iter().filter(|o| matches!(o, Op::AllocArenaRecord { .. })).count();
    let stack = h.code.iter().filter(|o| matches!(o, Op::AllocStackRecord { .. })).count();
    let heap = h.code.iter().filter(|o| matches!(o, Op::MakeRecord { .. })).count();
    eprintln!("[diag] handle() record sites: arena={arena}, stack={stack}, heap={heap}");

    let mut acc = 0i64;
    let mut tot_arena = 0u64;
    let mut tot_arena_fb = 0u64;
    for _ in 0..iters {
        let mut vm = Vm::new(&p);
        vm.set_step_limit(u64::MAX);
        let r = if arena_scope {
            let scope = vm.enter_request_scope();
            let r = vm.call("drive", vec![Value::Int(n)]);
            let r = r.map(|v| vm.materialize_arena_handles(v));
            vm.exit_request_scope(scope);
            r
        } else {
            vm.call("drive", vec![Value::Int(n)])
        };
        if let Value::Int(v) = r.unwrap() {
            acc = acc.wrapping_add(v);
        }
        tot_arena += vm.arena_record_allocs;
        tot_arena_fb += vm.arena_record_heap_fallbacks;
    }
    eprintln!("[diag] arena_scope_mode={arena_scope}");
    eprintln!("[diag] counters: arena={tot_arena}, arena_fallbacks={tot_arena_fb}");
    std::process::exit((acc & 0x7f) as i32);
}
