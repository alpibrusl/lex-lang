//! #463 slab-direct wire-up measurement.
//!
//! Compares the **per-request boundary cost** of two response-reading
//! patterns on a typical `Response { status, body, total }` handler:
//!
//! - **Old path** (pre-#595): `vm.materialize_arena_handles(r)` walks
//!   the tree allocating a heap `Box<IndexMap>` mirror, then the
//!   runtime reads three fields via `IndexMap::get`.
//! - **New path** (post-#595): `vm.get_record_field(r, name)` reads
//!   straight out of `arena_slab` — no materialize walk, no `IndexMap`
//!   alloc.
//!
//! Both arms invoke an arena-eligible handler inside an active scope
//! so the response is a `Value::ArenaRecord` (the case the wire-up
//! actually affects). The arena-off baseline isn't interesting here:
//! when arena is off the response is already a heap `Value::Record`
//! and both arms degenerate to the same `IndexMap::get` lookups.
//!
//! Run under callgrind:
//!   cargo build --release --example profile_unpack_response -p lex-bytecode
//!   valgrind --tool=callgrind --callgrind-out-file=cg.materialize.out \
//!     target/release/examples/profile_unpack_response 10000
//!   LEX_PROFILE_SLAB_DIRECT=1 valgrind --tool=callgrind \
//!     --callgrind-out-file=cg.slab.out \
//!     target/release/examples/profile_unpack_response 10000
//!
//! Arg: <iters>. Default 10000.

use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Value};
use lex_syntax::parse_source;

const SRC: &str = r#"
type Response = { status :: Int, body :: Str, total :: Int }

fn handler() -> Response {
  { status: 200, body: "hello", total: 42 }
}
"#;

fn main() {
    let iters: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let slab_direct = std::env::var_os("LEX_PROFILE_SLAB_DIRECT").is_some();

    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));
    let handler_id = p.function_names["handler"];

    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    let mut acc = 0i64;

    for _ in 0..iters {
        let scope = vm.enter_request_scope();
        let resp = vm.invoke(handler_id, vec![]).unwrap();
        debug_assert!(matches!(resp, Value::ArenaRecord { .. }),
            "expected arena handle under active scope");

        if slab_direct {
            // New path: read each field directly from the slab. No
            // materialize walk, no IndexMap allocation.
            let status = vm.get_record_field(&resp, "status")
                .and_then(|v| if let Value::Int(n) = v { Some(n) } else { None })
                .unwrap_or(0);
            let body_len = match vm.get_record_field(&resp, "body") {
                Some(Value::Str(s)) => s.len() as i64,
                _ => 0,
            };
            let total = vm.get_record_field(&resp, "total")
                .and_then(|v| if let Value::Int(n) = v { Some(n) } else { None })
                .unwrap_or(0);
            acc = acc.wrapping_add(status).wrapping_add(body_len).wrapping_add(total);
        } else {
            // Old path: materialize the full tree first, then read
            // fields via IndexMap::get. This is exactly what
            // `materialize_response_body` + `unpack_response`
            // (pre-#595) did at the response boundary.
            let materialized = vm.materialize_arena_handles(resp);
            if let Value::Record { fields, .. } = &materialized {
                let status = fields.get("status")
                    .and_then(|v| if let Value::Int(n) = v { Some(*n) } else { None })
                    .unwrap_or(0);
                let body_len = match fields.get("body") {
                    Some(Value::Str(s)) => s.len() as i64,
                    _ => 0,
                };
                let total = fields.get("total")
                    .and_then(|v| if let Value::Int(n) = v { Some(*n) } else { None })
                    .unwrap_or(0);
                acc = acc.wrapping_add(status).wrapping_add(body_len).wrapping_add(total);
            }
        }

        vm.exit_request_scope(scope);
    }

    eprintln!("[diag] slab_direct={slab_direct}, iters={iters}, acc={acc}");
    eprintln!("[diag] arena_allocs={}, arena_fallbacks={}",
        vm.arena_record_allocs, vm.arena_record_heap_fallbacks);
    std::process::exit((acc & 0x7f) as i32);
}
