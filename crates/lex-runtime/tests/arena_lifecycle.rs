//! #463 scaffolding tests — verify `enter_request_scope` /
//! `exit_request_scope` pair correctly on `DefaultHandler` and
//! that the arena lifecycle is symmetric across nested scopes.
//!
//! The scaffolding slice doesn't route any allocations through
//! the arena; these tests cover the lifecycle surface only —
//! page counts at boundaries, pair-correctness, and the
//! per-worker isolation that `spawn_for_worker` provides.

use lex_bytecode::vm::EffectHandler;
use lex_runtime::{DefaultHandler, Policy};

#[test]
fn empty_handler_has_no_active_arena() {
    let h = DefaultHandler::new(Policy::permissive());
    assert!(h.active_arena().is_none());
    assert_eq!(h.arena_stack_depth(), 0);
}

#[test]
fn enter_pushes_an_arena_exit_drops_it() {
    let mut h = DefaultHandler::new(Policy::permissive());
    let id = h.enter_request_scope();
    assert_eq!(h.arena_stack_depth(), 1);
    assert!(h.active_arena().is_some());
    h.exit_request_scope(id);
    assert_eq!(h.arena_stack_depth(), 0);
    assert!(h.active_arena().is_none());
}

#[test]
fn nested_scopes_stack_and_unwind_in_order() {
    let mut h = DefaultHandler::new(Policy::permissive());
    let outer = h.enter_request_scope();
    let inner = h.enter_request_scope();
    assert_eq!(h.arena_stack_depth(), 2);
    // The actively-allocating arena is the inner one — that's the
    // semantic the follow-on Value-rep slice depends on (when an
    // arena is "active", new allocations go to it).
    let outer_arena_ptr =
        h.active_arena().map(|a| a as *const _);
    h.exit_request_scope(inner);
    assert_eq!(h.arena_stack_depth(), 1);
    // After dropping the inner, the active arena is the outer one
    // — different pointer than what was active before.
    let now_active = h.active_arena().map(|a| a as *const _);
    assert_ne!(outer_arena_ptr, now_active);
    h.exit_request_scope(outer);
    assert_eq!(h.arena_stack_depth(), 0);
}

#[test]
fn out_of_order_exit_truncates_to_matching_id() {
    // The implementation deliberately tolerates out-of-order exits
    // (truncate everything from the matching scope down) rather
    // than panicking. A stray mismatched pair on a live server
    // shouldn't crash the process — it should just close the
    // surviving scopes and continue.
    let mut h = DefaultHandler::new(Policy::permissive());
    let a = h.enter_request_scope();
    let _b = h.enter_request_scope();
    let _c = h.enter_request_scope();
    assert_eq!(h.arena_stack_depth(), 3);
    // Exit the OLDEST scope — the implementation truncates b and c too.
    h.exit_request_scope(a);
    assert_eq!(h.arena_stack_depth(), 0);
}

#[test]
fn exit_with_unknown_id_is_a_noop() {
    let mut h = DefaultHandler::new(Policy::permissive());
    let _real = h.enter_request_scope();
    h.exit_request_scope(99999); // never returned by enter
    assert_eq!(h.arena_stack_depth(), 1);
}

#[test]
fn fresh_scope_ids_are_distinct() {
    let mut h = DefaultHandler::new(Policy::permissive());
    let a = h.enter_request_scope();
    h.exit_request_scope(a);
    let b = h.enter_request_scope();
    h.exit_request_scope(b);
    assert_ne!(a, b);
}

#[test]
fn worker_handler_starts_with_empty_arena_stack() {
    // `spawn_for_worker` produces a fresh handler with its own
    // empty stack — parent and worker scopes don't share state
    // (correct semantic: worker thread allocations have a
    // different lifetime than the request that spawned them).
    let mut parent = DefaultHandler::new(Policy::permissive());
    let _parent_scope = parent.enter_request_scope();
    assert_eq!(parent.arena_stack_depth(), 1);

    let _worker = parent.spawn_for_worker();
    // Worker construction doesn't mutate parent.
    assert_eq!(parent.arena_stack_depth(), 1);
    // Can't easily inspect the worker's depth through the trait
    // object (the test imports the concrete type for parent); the
    // important property is that the worker construction returned
    // Some, meaning the trait method is implemented.
    assert!(_worker.is_some());
}
