//! #463 slice 2a — `Op::AllocArenaRecord` / `Op::AllocArenaTuple`
//! end-to-end tests.
//!
//! Slice 2a ships the VM-side machinery without a codegen lowering
//! pass, so all programs here are hand-crafted bytecode. Once
//! slice 2b adds `apply_arena_lowering` (compiler integration), a
//! sibling source-level suite (paralleling `stack_records.rs`) will
//! exercise the lowering and the polymorphic dispatch end-to-end.
//!
//! What this suite covers:
//! - alloc inside an active request scope → arena slab path,
//!   `arena_record_allocs` counter
//! - alloc outside any scope → heap fallback to `MakeRecord` /
//!   `MakeTuple` shape, `arena_record_heap_fallbacks` counter
//! - `exit_request_scope` truncates the slab in O(1)
//! - LIFO nesting: inner exit only releases inner allocations
//! - `Op::GetField` polymorphic over `Record` / `StackRecord` /
//!   `ArenaRecord` via the shared IC slot
//! - `Op::GetElem` polymorphic over `Tuple` / `StackTuple` /
//!   `ArenaTuple`
//! - `body_hash` invariance (#222) — the new ops hash bit-identically
//!   to their `MakeRecord` / `MakeTuple` forms, so closure identity
//!   survives the future lowering

use std::sync::Arc;

use indexmap::IndexMap;
use lex_bytecode::{Const, Op, Program, Value, Vm};
use lex_bytecode::program::{compute_body_hash, Function, ZERO_BODY_HASH};

/// Build a single-function `Program` with `code` as the body and one
/// record shape (a 2-field `{x, y}` shape at index 0). Caller's code
/// can reference field-name consts at indices 0 (`"x"`) and 1
/// (`"y"`); integer literals start at index 2.
fn xy_program(name: &str, locals_count: u16, code: Vec<Op>) -> Arc<Program> {
    let constants = vec![
        Const::FieldName("x".into()),
        Const::FieldName("y".into()),
        Const::Int(7),
        Const::Int(9),
    ];
    let mut function_names = IndexMap::new();
    function_names.insert(name.to_string(), 0);
    Arc::new(Program {
        constants,
        functions: vec![Function {
            name: name.into(),
            arity: 0,
            locals_count,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 4, // upper bound — actual sites < 4
        }],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![vec![0, 1]], // `{x, y}` shape
    })
}

/// Build a record-free single-function `Program` for tuple tests.
fn tup_program(name: &str, locals_count: u16, code: Vec<Op>) -> Arc<Program> {
    let constants = vec![Const::Int(11), Const::Int(13)];
    let mut function_names = IndexMap::new();
    function_names.insert(name.to_string(), 0);
    Arc::new(Program {
        constants,
        functions: vec![Function {
            name: name.into(),
            arity: 0,
            locals_count,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    })
}

// ---------------------------------------------------------------
// Alloc + read inside an active scope
// ---------------------------------------------------------------

#[test]
fn arena_alloc_inside_scope_routes_to_slab() {
    // fn() -> Int { let r = {x:7, y:9}; r.x }   ← but with AllocArenaRecord
    let code = vec![
        Op::PushConst(2),                                       // 7
        Op::PushConst(3),                                       // 9
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },  // → ArenaRecord
        Op::GetField { name_idx: 0, site_idx: 0 },              // .x
        Op::Return,
    ];
    let prog = xy_program("read_x", 0, code);
    let mut vm = Vm::new(&prog);

    let scope = vm.enter_request_scope();
    let result = vm.invoke(0, vec![]).unwrap();
    vm.exit_request_scope(scope);

    assert_eq!(result, Value::Int(7));
    assert_eq!(vm.arena_record_allocs, 1);
    assert_eq!(vm.arena_record_heap_fallbacks, 0);
}

// ---------------------------------------------------------------
// No active scope → heap fallback
// ---------------------------------------------------------------

#[test]
fn arena_alloc_outside_scope_falls_back_to_heap() {
    let code = vec![
        Op::PushConst(2),
        Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Return,
    ];
    let prog = xy_program("alloc_no_scope", 0, code);
    let mut vm = Vm::new(&prog);

    // Deliberately no enter_request_scope.
    let result = vm.invoke(0, vec![]).unwrap();

    // Fallback produces a plain heap Record indistinguishable from
    // what MakeRecord would have made — same shape, same field
    // ordering, same observable equality.
    match &result {
        Value::Record { shape_id, fields } => {
            assert_eq!(*shape_id, 0);
            assert_eq!(fields.len(), 2);
            assert_eq!(fields.get("x"), Some(&Value::Int(7)));
            assert_eq!(fields.get("y"), Some(&Value::Int(9)));
        }
        other => panic!("expected fallback Value::Record, got {other:?}"),
    }
    assert_eq!(vm.arena_record_allocs, 0);
    assert_eq!(vm.arena_record_heap_fallbacks, 1);
}

#[test]
fn arena_tuple_alloc_outside_scope_falls_back_to_heap() {
    let code = vec![
        Op::PushConst(0), // 11
        Op::PushConst(1), // 13
        Op::AllocArenaTuple { arity: 2 },
        Op::Return,
    ];
    let prog = tup_program("tup_no_scope", 0, code);
    let mut vm = Vm::new(&prog);
    let result = vm.invoke(0, vec![]).unwrap();
    assert_eq!(result, Value::Tuple(vec![Value::Int(11), Value::Int(13)]));
    assert_eq!(vm.arena_record_heap_fallbacks, 1);
}

// ---------------------------------------------------------------
// Slab truncation on exit
// ---------------------------------------------------------------

#[test]
fn exit_request_scope_truncates_slab() {
    // Build a value tree inside the scope (3 records → 6 slots),
    // discard it, exit, verify slab is empty. The records are
    // dropped before exit so nothing is reachable; the truncation
    // is what releases the slab storage.
    let code = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Pop,
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Pop,
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Pop,
        Op::PushConst(2),
        Op::Return,
    ];
    let prog = xy_program("three_drops", 0, code);
    let mut vm = Vm::new(&prog);

    let scope = vm.enter_request_scope();
    let r = vm.invoke(0, vec![]).unwrap();
    assert_eq!(r, Value::Int(7));
    assert_eq!(vm.arena_record_allocs, 3);
    vm.exit_request_scope(scope);

    // arena_slab is private; the observable proxy is that a fresh
    // alloc inside a new scope starts at slab_start=0 again. We
    // verify that indirectly by entering a new scope and observing
    // the alloc counter / read working from a fresh slab.
    let scope2 = vm.enter_request_scope();
    let r2 = vm.invoke(0, vec![]).unwrap();
    assert_eq!(r2, Value::Int(7));
    assert_eq!(vm.arena_record_allocs, 6); // 3 more
    vm.exit_request_scope(scope2);
}

// ---------------------------------------------------------------
// Nested LIFO scopes
// ---------------------------------------------------------------

#[test]
fn nested_scopes_pop_in_lifo_order() {
    let code = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::GetField { name_idx: 0, site_idx: 0 },
        Op::Return,
    ];
    let prog = xy_program("alloc_and_read", 0, code);
    let mut vm = Vm::new(&prog);

    let outer = vm.enter_request_scope();
    let r1 = vm.invoke(0, vec![]).unwrap();
    assert_eq!(r1, Value::Int(7));

    // Inner scope: a separate alloc. Exiting the inner scope must
    // release only the inner's slab usage; the outer's allocations
    // (from above) are no longer reachable since the producing
    // frame returned, but slab storage remains held until outer exit.
    let inner = vm.enter_request_scope();
    let r2 = vm.invoke(0, vec![]).unwrap();
    assert_eq!(r2, Value::Int(7));
    vm.exit_request_scope(inner);

    // Outer still active — another alloc must succeed and route to
    // arena (not heap fallback).
    let allocs_before = vm.arena_record_allocs;
    let _ = vm.invoke(0, vec![]).unwrap();
    assert_eq!(vm.arena_record_allocs, allocs_before + 1);
    assert_eq!(vm.arena_record_heap_fallbacks, 0);

    vm.exit_request_scope(outer);

    // After outer exit, a fresh alloc with no scope falls back.
    let _ = vm.invoke(0, vec![]).unwrap();
    assert_eq!(vm.arena_record_heap_fallbacks, 1);
}

// ---------------------------------------------------------------
// Polymorphic GetField over Record / ArenaRecord
// ---------------------------------------------------------------

#[test]
fn polymorphic_get_field_arena_record_caches_then_hits() {
    // Call twice — first call installs the IC, second hits it. Both
    // return the same field value.
    let code = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::GetField { name_idx: 1, site_idx: 0 },  // .y
        Op::Return,
    ];
    let prog = xy_program("read_y_twice", 0, code);
    let mut vm = Vm::new(&prog);

    let scope = vm.enter_request_scope();
    let a = vm.invoke(0, vec![]).unwrap();
    let b = vm.invoke(0, vec![]).unwrap();
    vm.exit_request_scope(scope);

    assert_eq!(a, Value::Int(9));
    assert_eq!(b, Value::Int(9));
    assert_eq!(vm.arena_record_allocs, 2);
}

// ---------------------------------------------------------------
// Tuple alloc + GetElem inside scope
// ---------------------------------------------------------------

#[test]
fn arena_tuple_alloc_and_get_elem_inside_scope() {
    let code = vec![
        Op::PushConst(0),  // 11
        Op::PushConst(1),  // 13
        Op::AllocArenaTuple { arity: 2 },
        Op::GetElem(1),    // → 13
        Op::Return,
    ];
    let prog = tup_program("tup_read", 0, code);
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let r = vm.invoke(0, vec![]).unwrap();
    vm.exit_request_scope(scope);
    assert_eq!(r, Value::Int(13));
    assert_eq!(vm.arena_record_allocs, 1);
}

// ---------------------------------------------------------------
// body_hash invariance — #222 contract
// ---------------------------------------------------------------

#[test]
fn body_hash_invariance_record() {
    // Two function bodies, identical except one uses MakeRecord and
    // the other uses AllocArenaRecord at the same site. The body
    // hash must be bit-identical so closure identity (#222) survives
    // the future lowering.
    let make = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::MakeRecord { shape_idx: 0, field_count: 2 },
        Op::Return,
    ];
    let arena = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Return,
    ];
    let shapes: Vec<Vec<u32>> = vec![vec![0, 1]];
    let h_make = compute_body_hash(0, 0, &make, &shapes);
    let h_arena = compute_body_hash(0, 0, &arena, &shapes);
    assert_eq!(h_make, h_arena,
        "AllocArenaRecord must hash as MakeRecord for #222 closure identity");

    // And both still match `AllocStackRecord` — the three are
    // interchangeable at the hash level, which is the precondition
    // for letting the lowering pass route a given site to any of
    // them without disturbing downstream closures.
    let stack = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocStackRecord { shape_idx: 0, field_count: 2 },
        Op::Return,
    ];
    assert_eq!(h_make, compute_body_hash(0, 0, &stack, &shapes));
}

#[test]
fn body_hash_invariance_tuple() {
    let make = vec![
        Op::PushConst(0), Op::PushConst(1),
        Op::MakeTuple(2),
        Op::Return,
    ];
    let arena = vec![
        Op::PushConst(0), Op::PushConst(1),
        Op::AllocArenaTuple { arity: 2 },
        Op::Return,
    ];
    let shapes: Vec<Vec<u32>> = vec![];
    assert_eq!(compute_body_hash(0, 0, &make, &shapes),
               compute_body_hash(0, 0, &arena, &shapes),
               "AllocArenaTuple must hash as MakeTuple for #222 closure identity");
}

// ---------------------------------------------------------------
// Round-trip via local storage
// ---------------------------------------------------------------

#[test]
fn arena_record_round_trips_through_local() {
    // Build, store, load, read field — the typical handler shape
    // (Response built into a local, then returned via the local).
    let code = vec![
        Op::PushConst(2), Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::StoreLocal(0),
        Op::LoadLocal(0),
        Op::GetField { name_idx: 1, site_idx: 0 }, // .y
        Op::Return,
    ];
    let prog = xy_program("roundtrip", 1, code);
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let r = vm.invoke(0, vec![]).unwrap();
    vm.exit_request_scope(scope);
    assert_eq!(r, Value::Int(9));
}

// ---------------------------------------------------------------
// Vm::materialize_arena_handles — the boundary helper (slice 2a-iii)
// ---------------------------------------------------------------

/// Helper: build a handler that returns its arena record without
/// destructuring it (so we can hand the raw handle to materialize).
fn handler_returning_xy_record() -> Arc<Program> {
    let code = vec![
        Op::PushConst(2),
        Op::PushConst(3),
        Op::AllocArenaRecord { shape_idx: 0, field_count: 2 },
        Op::Return,
    ];
    xy_program("build_xy", 0, code)
}

#[test]
fn materialize_passthrough_primitives() {
    let prog = xy_program("noop", 0, vec![Op::PushConst(2), Op::Return]);
    let vm = Vm::new(&prog);
    assert_eq!(vm.materialize_arena_handles(Value::Int(42)), Value::Int(42));
    assert_eq!(vm.materialize_arena_handles(Value::Bool(true)), Value::Bool(true));
    assert_eq!(vm.materialize_arena_handles(Value::Unit), Value::Unit);
}

#[test]
fn materialize_passthrough_heap_record() {
    // A Value::Record already in heap form materializes to itself
    // (idempotency over the no-arena case).
    let prog = xy_program("noop", 0, vec![Op::PushConst(2), Op::Return]);
    let vm = Vm::new(&prog);
    let mut fields: IndexMap<smol_str::SmolStr, Value> = IndexMap::new();
    fields.insert("x".into(), Value::Int(7));
    fields.insert("y".into(), Value::Int(9));
    let v = Value::Record { shape_id: 0, fields: Box::new(fields) };
    assert_eq!(vm.materialize_arena_handles(v.clone()), v);
}

#[test]
fn materialize_arena_record_becomes_heap_record() {
    let prog = handler_returning_xy_record();
    let mut vm = Vm::new(&prog);

    let scope = vm.enter_request_scope();
    let arena_value = vm.invoke(0, vec![]).unwrap();
    // The handler returned a raw ArenaRecord — confirm the shape
    // before we materialize so the assertion is meaningful.
    assert!(matches!(arena_value, Value::ArenaRecord { .. }));

    let heap_value = vm.materialize_arena_handles(arena_value);
    // Now we can safely exit the scope — the materialized value is
    // independent of the slab.
    vm.exit_request_scope(scope);

    match heap_value {
        Value::Record { shape_id, fields } => {
            assert_eq!(shape_id, 0);
            assert_eq!(fields.get("x"), Some(&Value::Int(7)));
            assert_eq!(fields.get("y"), Some(&Value::Int(9)));
        }
        other => panic!("expected materialized Value::Record, got {other:?}"),
    }
}

#[test]
fn materialize_arena_tuple_becomes_heap_tuple() {
    let prog = tup_program("build_tup", 0, vec![
        Op::PushConst(0), Op::PushConst(1),
        Op::AllocArenaTuple { arity: 2 },
        Op::Return,
    ]);
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let arena_value = vm.invoke(0, vec![]).unwrap();
    assert!(matches!(arena_value, Value::ArenaTuple { .. }));
    let heap_value = vm.materialize_arena_handles(arena_value);
    vm.exit_request_scope(scope);
    assert_eq!(heap_value, Value::Tuple(vec![Value::Int(11), Value::Int(13)]));
}

#[test]
fn materialize_recurses_into_list_elements() {
    // A heap list containing arena records — confirm the walk
    // descends into the list. (Hand-constructed because no op
    // currently builds a list of arena handles, but the helper
    // must handle it for slice-2b safety.)
    let prog = handler_returning_xy_record();
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let arena_a = vm.invoke(0, vec![]).unwrap();
    let arena_b = vm.invoke(0, vec![]).unwrap();
    let list = Value::List([arena_a, arena_b].into_iter().collect());
    let materialized = vm.materialize_arena_handles(list);
    vm.exit_request_scope(scope);
    match materialized {
        Value::List(items) => {
            assert_eq!(items.len(), 2);
            for item in items {
                assert!(matches!(item, Value::Record { .. }),
                    "list element should be heap Record, got {item:?}");
            }
        }
        other => panic!("expected Value::List, got {other:?}"),
    }
}

#[test]
fn materialize_recurses_into_record_field_value() {
    // Heap Record whose field value is an arena handle — confirms
    // we walk into Record fields (the common case once codegen
    // mixes arena children inside non-arena parents).
    let prog = handler_returning_xy_record();
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let inner = vm.invoke(0, vec![]).unwrap(); // ArenaRecord
    let mut fields: IndexMap<smol_str::SmolStr, Value> = IndexMap::new();
    fields.insert("nested".into(), inner);
    let outer = Value::Record { shape_id: 0, fields: Box::new(fields) };
    let materialized = vm.materialize_arena_handles(outer);
    vm.exit_request_scope(scope);
    match materialized {
        Value::Record { fields, .. } => {
            let nested = fields.get("nested").expect("nested field present");
            assert!(matches!(nested, Value::Record { .. }),
                "nested field should be heap Record after materialization, got {nested:?}");
        }
        other => panic!("expected Value::Record outer, got {other:?}"),
    }
}

#[test]
fn materialize_to_json_roundtrip_does_not_panic() {
    // The whole reason this helper exists: an arena value can't be
    // to_json'd directly (defensive panic), but materialize-then-
    // to_json works. This pair is exactly the response-serialization
    // pattern slice 2b will wire up.
    let prog = handler_returning_xy_record();
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let arena_value = vm.invoke(0, vec![]).unwrap();
    let heap_value = vm.materialize_arena_handles(arena_value);
    vm.exit_request_scope(scope);

    let json = heap_value.to_json();
    assert_eq!(json["x"], serde_json::json!(7));
    assert_eq!(json["y"], serde_json::json!(9));
}

#[test]
fn materialize_is_idempotent() {
    // Materialize twice — second pass walks a tree with no handles,
    // which must return an equivalent value (clones notwithstanding).
    let prog = handler_returning_xy_record();
    let mut vm = Vm::new(&prog);
    let scope = vm.enter_request_scope();
    let arena_value = vm.invoke(0, vec![]).unwrap();
    let once = vm.materialize_arena_handles(arena_value);
    let twice = vm.materialize_arena_handles(once.clone());
    vm.exit_request_scope(scope);
    assert_eq!(once, twice);
}
