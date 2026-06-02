//! M5: bytecode VM. Stack machine with effect dispatch through a host handler.

use crate::op::*;
use crate::program::*;
use crate::value::{ActorCell, Value};
use std::sync::{Arc, Mutex, OnceLock};
use indexmap::IndexMap;
use smol_str::SmolStr;
use std::collections::{HashMap, VecDeque};

// ── IC polymorphism instrumentation (throwaway, env-gated) ─────────
// Enable with LEX_IC_STATS=1. With LEX_IC_STATS_OUT=<path> writes a
// TSV to <path>.<pid> on each Vm drop; otherwise dumps to stderr.

#[derive(Default)]
struct IcStats {
    sites: HashMap<(u32, u32), HashMap<u32, u64>>,
}

static IC_STATS: OnceLock<Mutex<IcStats>> = OnceLock::new();
static IC_STATS_ENABLED: OnceLock<bool> = OnceLock::new();

fn ic_stats_enabled() -> bool {
    *IC_STATS_ENABLED.get_or_init(|| {
        std::env::var("LEX_IC_STATS").map(|v| v == "1").unwrap_or(false)
    })
}

fn record_ic_hit(fn_id: u32, site_idx: u32, shape_id: u32) {
    let stats = IC_STATS.get_or_init(|| Mutex::new(IcStats::default()));
    let mut s = stats.lock().unwrap();
    *s.sites.entry((fn_id, site_idx)).or_default().entry(shape_id).or_insert(0) += 1;
}

pub fn dump_ic_stats() {
    let Some(stats) = IC_STATS.get() else { return; };
    let s = stats.lock().unwrap();
    if s.sites.is_empty() { return; }
    let mut out = String::from("fn_id\tsite_idx\tshape_id\thits\n");
    let mut entries: Vec<_> = s.sites.iter().collect();
    entries.sort_by_key(|((f, si), _)| (*f, *si));
    for ((f, site), shapes) in entries {
        let mut shape_entries: Vec<_> = shapes.iter().collect();
        shape_entries.sort_by_key(|(sid, _)| **sid);
        for (sid, hits) in shape_entries {
            out.push_str(&format!("{f}\t{site}\t{sid}\t{hits}\n"));
        }
    }
    match std::env::var("LEX_IC_STATS_OUT").ok() {
        Some(path) => {
            let pid = std::process::id();
            let _ = std::fs::write(format!("{path}.{pid}"), out);
        }
        None => { eprint!("{out}"); }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum VmError {
    #[error("runtime panic: {0}")]
    Panic(String),
    #[error("type mismatch at runtime: {0}")]
    TypeMismatch(String),
    #[error("stack underflow")]
    StackUnderflow,
    #[error("unknown function: {0}")]
    UnknownFunction(String),
    #[error("effect handler error: {0}")]
    Effect(String),
    #[error("call stack overflow: recursion depth exceeded ({0})")]
    CallStackOverflow(u32),
    /// Refinement predicate failed at a call boundary (#209 slice 3).
    /// Surfaced when a function declares `param :: Type{x | predicate}`,
    /// the call-site arg couldn't be discharged statically (slice 2),
    /// and the runtime evaluator finds the predicate is `false` for
    /// the actual argument value. The `verdict` mirrors the shape of
    /// `gate.verdict`-style records in `lex-trace`.
    #[error("refinement violated: argument {param_index} of `{fn_name}` (binding `{binding}`): {reason}")]
    RefinementFailed {
        fn_name: String,
        param_index: usize,
        binding: String,
        reason: String,
    },
}

/// Maximum simultaneous call frames. Defends against unbounded
/// recursion in agent-emitted code: a body that calls itself
/// without a base case would otherwise blow the host's native
/// stack and crash the process. Real Lex code rarely exceeds
/// ~30 frames; 1024 is generous headroom while still well under
/// the OS stack limit at any per-frame size we use.
pub const MAX_CALL_DEPTH: u32 = 1024;

/// Per-frame stack-record budget (#464 step 2). Counts the number of
/// `Value` slots a frame may consume from `Vm::stack_record_arena`
/// before further `Op::AllocStackRecord` requests fall back to the
/// heap path. 64 slots at the current `size_of::<Value>() = 64B`
/// gives ~4 KiB per frame, matching the design-doc proposal in
/// `docs/design/escape-analysis.md`. A handler-shaped function
/// (one outer record of ≤8 fields, plus a handful of small inner
/// records) fits well inside this without growing.
pub const STACK_RECORD_BUDGET_SLOTS: u32 = 64;

/// Adaptive-memoization warmup window (#229 adaptive). A pure
/// function is given this many cache-probing calls to demonstrate a
/// hit; if it reaches the window with zero hits, memoization is
/// disabled for it (its calls stop hashing args). A function that
/// genuinely benefits — e.g. naive recursive `fib`, where each call
/// immediately reuses sub-results — accumulates hits well before the
/// window closes and stays enabled. 64 balances "give real reuse a
/// chance" against "don't pay the hash forever on always-miss code".
const MEMO_WARMUP_CALLS: u32 = 64;

/// Per-function adaptive-memoization state (#229 adaptive). `enabled`
/// starts true; once a function reaches `MEMO_WARMUP_CALLS` cache
/// probes with `hits == 0`, it flips to false and that function's
/// calls skip the args hash entirely for the rest of the Vm's life.
#[derive(Clone, Copy)]
struct MemoFnState {
    calls: u32,
    hits: u32,
    enabled: bool,
}

impl Default for MemoFnState {
    fn default() -> Self {
        MemoFnState { calls: 0, hits: 0, enabled: true }
    }
}

/// Host-side effect dispatch. Implementors decide what `kind`/`op` mean
/// and how arguments map to side effects.
pub trait EffectHandler {
    fn dispatch(&mut self, kind: &str, op: &str, args: Vec<Value>) -> Result<Value, String>;

    /// Hook called by the VM at every function call so handlers can
    /// enforce per-call budget consumption (#225). The argument is
    /// the sum of `[budget(N)]` declared on the callee's signature;
    /// the handler returns `Err` to refuse the call (the VM converts
    /// to `VmError::Effect`). Default impl is a no-op so legacy
    /// handlers and pure-only runs are unaffected.
    fn note_call_budget(&mut self, _budget_cost: u64) -> Result<(), String> {
        Ok(())
    }

    /// Enter a per-request allocation scope (#463 scaffolding).
    /// Called by the runtime layer (e.g. `net.serve_fn`'s request
    /// loop) immediately before invoking the user handler closure
    /// for one request. Implementations push a fresh arena onto
    /// their internal stack and return its identifier; the matching
    /// `exit_request_scope` call drops it.
    ///
    /// Default impl is a no-op — handlers without arena support
    /// return a sentinel scope id which they ignore on exit.
    /// `DefaultHandler` in `lex-runtime` provides the real
    /// implementation.
    ///
    /// Today the VM does NOT route any `Value` allocations through
    /// the returned arena — see the scaffolding notes in
    /// `crates/lex-runtime/src/arena.rs`. The hook exists so the
    /// follow-on slice that adds Value-rep arena routing has a
    /// stable trait surface to extend.
    fn enter_request_scope(&mut self) -> u64 { 0 }

    /// Exit a per-request allocation scope opened by
    /// `enter_request_scope`. Implementations drop the arena
    /// associated with `scope_id`. Calling exit with a scope_id
    /// that wasn't returned by a prior enter is implementation-
    /// defined behavior — DefaultHandler treats it as a no-op so
    /// mismatched pairs don't panic.
    fn exit_request_scope(&mut self, _scope_id: u64) {}

    /// `list.par_map` worker-handler factory (#305 slice 2).
    ///
    /// Each parallel worker thread runs its own `Vm` and therefore
    /// needs its own effect handler. The parent handler may opt in
    /// to per-worker dispatch by returning `Some(handler)` here;
    /// returning `None` (the default) keeps slice-1 behavior: the
    /// worker runs `DenyAllEffects` and any effect call inside the
    /// closure fails with `VmError::Effect`.
    ///
    /// The returned handler must be `Send` so the worker can take
    /// ownership across a thread boundary. Shared state (budget
    /// pool, chat registry, etc.) is wired up by the implementer.
    /// Per-worker independence (MCP client cache, output sink)
    /// is intentional — the alternative is mutex-serialization of
    /// the whole effect dispatch, which would defeat the parallelism.
    fn spawn_for_worker(&self) -> Option<Box<dyn EffectHandler + Send>> {
        None
    }
}

/// `Vm` exposes itself as a `ClosureCaller` so the parser interpreter
/// can invoke user-supplied closures during a `parser.run` walk
/// (#221). The Vm is reentrant for closure invocation: pushing a new
/// frame onto an active call stack is supported, and the handler
/// stays in place so any effects the closure body fires dispatch
/// normally.
impl<'a> crate::parser_runtime::ClosureCaller for Vm<'a> {
    fn call_closure(&mut self, closure: Value, args: Vec<Value>) -> Result<Value, String> {
        self.invoke_closure_value(closure, args)
            .map_err(|e| format!("{e:?}"))
    }
}

/// A handler that fails any effect call. Useful as a default for pure-only runs.
pub struct DenyAllEffects;
impl EffectHandler for DenyAllEffects {
    fn dispatch(&mut self, kind: &str, op: &str, _args: Vec<Value>) -> Result<Value, String> {
        Err(format!("effects not permitted (attempted {kind}.{op})"))
    }
}

/// Trace receiver. Implementors record the call/effect tree and may
/// substitute effect responses (for replay).
pub trait Tracer {
    fn enter_call(&mut self, node_id: &str, name: &str, args: &[Value]);
    fn enter_effect(&mut self, node_id: &str, kind: &str, op: &str, args: &[Value]);
    fn exit_ok(&mut self, value: &Value);
    fn exit_err(&mut self, message: &str);
    /// Tail-call optimization: pop the current frame's open call without
    /// re-entering the parent (the new call takes its place).
    fn exit_call_tail(&mut self);
    /// During replay, return Some(v) to substitute an effect's output.
    fn override_effect(&mut self, _node_id: &str) -> Option<Value> { None }
}

/// No-op tracer for normal execution.
pub struct NullTracer;
impl Tracer for NullTracer {
    fn enter_call(&mut self, _: &str, _: &str, _: &[Value]) {}
    fn enter_effect(&mut self, _: &str, _: &str, _: &str, _: &[Value]) {}
    fn exit_ok(&mut self, _: &Value) {}
    fn exit_err(&mut self, _: &str) {}
    fn exit_call_tail(&mut self) {}
}

#[derive(Debug, Clone)]
pub(crate) enum FrameKind {
    /// Top-level entry frame; doesn't correspond to a Call opcode.
    Entry,
    /// Frame opened by Call/TailCall. The `String` is the originating
    /// `NodeId`; useful for diagnostics even if currently unread.
    Call(#[allow(dead_code)] String),
}

pub struct Vm<'a> {
    program: &'a Program,
    handler: Box<dyn EffectHandler + 'a>,
    pub(crate) tracer: Box<dyn Tracer + 'a>,
    /// Per-call frames. Each frame has its own locals array and pc.
    frames: Vec<Frame>,
    stack: Vec<Value>,
    /// Soft cap to avoid runaway computations in tests.
    pub step_limit: u64,
    pub steps: u64,
    /// Per-Vm memoization cache for pure functions (#229). Keyed by
    /// `(fn_id, hash_call_args(args))` — a 128-bit structural digest
    /// of the arguments (see `hash_call_args`). Effectful functions
    /// never enter this map. The cache lives for the lifetime of one
    /// `Vm::call` chain — calling `Vm::with_handler` again starts a
    /// fresh cache.
    pure_memo: std::collections::HashMap<(u32, [u8; 16]), Value>,
    /// Diagnostic counters for `--trace` observability (#229).
    pub pure_memo_hits: u64,
    pub pure_memo_misses: u64,
    /// Number of effect-free calls that skipped the cache entirely
    /// because adaptive memoization disabled their function (#229
    /// adaptive). Observability only.
    pub pure_memo_skips: u64,
    /// Adaptive-memoization state, one entry per function (indexed by
    /// `fn_id`), parallel to `field_ics` (#229 adaptive). Memoization
    /// only pays when a function is called repeatedly with equal args;
    /// the unconditional `hash_call_args` on every effect-free call is
    /// pure overhead otherwise (the `response_build` profile: 0 hits /
    /// 3600 misses, ~12% of instructions). After a warmup window with
    /// zero hits we stop hashing that function's calls — always safe,
    /// since the callee is pure and recomputing yields the same value.
    /// Sticky for the Vm's lifetime: a function that hasn't hit in
    /// `MEMO_WARMUP_CALLS` calls won't amortize later.
    memo_fn_state: Vec<MemoFnState>,
    /// Monomorphic inline caches for `Op::GetField` (#462 slice 1 +
    /// shape-keyed verification slice). Indexed by
    /// `[fn_id as usize][site_idx as usize]` — one entry per
    /// field-access site within each function. `site_idx` is assigned
    /// at compile time by `FnCompiler::field_get_sites` so every emit
    /// produces a stable identifier independent of pc. The cache
    /// survives the planned dispatch rewrite (#461) and a future
    /// JIT (#465).
    ///
    /// Slot shape: `(shape_id, offset)`. The pre-shape-keyed slice
    /// stored only the offset and re-verified each hit by walking
    /// `IndexMap::get_index(off)` and string-comparing the field name
    /// against the requested `name_idx`. After this slice, hits
    /// against compile-time records (real `shape_id`) verify with a
    /// single `u32` compare and skip the string compare entirely —
    /// per the #462 slice-2b measurement that observed 0% polymorphism
    /// and 86% of hits going to records with a real shape_id.
    ///
    /// `NO_SHAPE_ID` records (JSON / SQL / HTTP-built — 14% of measured
    /// hits, 100% of inbox/gateway traffic) fall through to the
    /// pre-slice name-compare verification. Distinct dynamic shapes
    /// both carry `NO_SHAPE_ID` and would otherwise alias on a
    /// pure-shape-keyed IC; keeping the name compare on that path
    /// preserves correctness without a separate cache for them.
    ///
    /// Outer Vec is pre-sized to `program.functions.len()`; each inner
    /// Vec is empty until the first GetField in that function runs,
    /// at which point we one-shot allocate it to the compiler-recorded
    /// `field_ic_sites` size and never resize again. Lazy on the inner
    /// side so VMs created for short-lived scripts don't eagerly
    /// allocate IC slots for functions they never enter.
    field_ics: Vec<Vec<Option<(u32, usize)>>>,
    /// Stack allocator for function locals (#389 slice 3).
    ///
    /// Every function frame claims `locals_count` contiguous slots from
    /// this Vec on push and releases them on pop.  Because Lex uses
    /// strictly LIFO frame semantics the most-recently-pushed frame's
    /// slots always sit at the top of the Vec, so `truncate` is the
    /// correct (and O(1)) release operation.
    ///
    /// The Vec is pre-allocated once at VM construction and then grows
    /// only if the actual call depth × locals width exceeds the initial
    /// capacity.  After a top-level `vm.call` returns the Vec is empty
    /// again but its capacity is retained, so the next request incurs
    /// zero allocations for locals up to the high-water mark.
    locals_storage: Vec<Value>,
    /// Stack-record arena (#464 step 2). Each `Op::AllocStackRecord`
    /// at a non-escaping site appends its `field_count` field values
    /// here; the produced `Value::StackRecord` carries `slab_start =
    /// arena.len() - field_count` so reads are an O(1) slab index.
    /// On `Op::Return` the arena is truncated back to
    /// `frame.stack_record_arena_start`, releasing every record the
    /// frame allocated in O(1) — same lifetime story as
    /// `locals_storage` for frame locals.
    ///
    /// LIFO frame discipline guarantees a frame's records always sit
    /// at the top of the arena while the frame is live, so neither
    /// inter-frame interleaving nor index churn can occur.
    stack_record_arena: Vec<Value>,
    /// Per-Vm counters for #464 acceptance measurement. Incremented
    /// on every `Op::MakeRecord` / `Op::AllocStackRecord` dispatch.
    /// The bench reads these to compute the stack-allocation rate
    /// (≥ 60% of records on the stack is the acceptance bar). Cheap
    /// in the hot path — two unconditional u64 increments per record.
    pub stack_record_allocs: u64,
    pub stack_record_heap_fallbacks: u64,
    pub heap_record_allocs: u64,
    /// Request-scoped arena slab (#463 slice 2a). Mirrors the shape of
    /// `stack_record_arena` but lives across frames inside the
    /// request scope opened by `EffectHandler::enter_request_scope`.
    /// Each `Op::AllocArenaRecord` / `Op::AllocArenaTuple` appends its
    /// field values here and pushes a handle (`Value::ArenaRecord` /
    /// `Value::ArenaTuple`) whose `slab_start` indexes back in.
    /// Truncated to the saved start on `exit_request_scope`, releasing
    /// every value the scope built in O(1) — same lifetime story as
    /// `stack_record_arena` truncating on `Op::Return`.
    ///
    /// Slabs nest LIFO: `arena_scope_starts` holds the
    /// `arena_slab.len()` snapshot taken at each `enter_request_scope`,
    /// and `exit_request_scope` truncates back to the matching entry.
    /// An empty `arena_scope_starts` means **no active scope** — the
    /// alloc ops fall back to their `MakeRecord` / `MakeTuple` heap
    /// path, so the VM stays sound when arena-lowered bytecode runs in
    /// a non-handler context.
    arena_slab: Vec<Value>,
    /// LIFO stack of `arena_slab.len()` snapshots, one per active
    /// request scope. See `arena_slab`.
    arena_scope_starts: Vec<u32>,
    /// Counters for #463 slice-2b acceptance (will be the
    /// arena-allocation-rate gate, paralleling the #464 stack-rate
    /// counters above). Incremented in the op handlers; harmless in
    /// slice 2a since codegen doesn't emit the ops yet.
    pub arena_record_allocs: u64,
    pub arena_record_heap_fallbacks: u64,
    /// Optional JIT tier hook (#465 phase-1 integration). Consulted
    /// by the `Op::Call` dispatch arm after refinements + memo. See
    /// `crate::jit_hook` for the trait contract. `None` means
    /// "interpreter-only" — that branch in the dispatch arm folds
    /// to a single null-pointer check the optimizer can hoist.
    jit_hook: Option<Box<dyn crate::jit_hook::JitHook + 'a>>,
}

struct Frame {
    fn_id: u32,
    pc: usize,
    /// Start index of this frame's locals in `Vm::locals_storage` (#389
    /// slice 3). The frame owns `locals_storage[locals_start..locals_start
    /// + locals_len]`; `Op::Return` truncates the Vec back to
    /// `locals_start`, releasing the slots in O(1).
    locals_start: usize,
    locals_len: usize,
    /// Stack base when this frame started (for cleanup on return).
    stack_base: usize,
    trace_kind: FrameKind,
    /// Pure-fn memo key (#229). `Some(key)` if the call was eligible
    /// for memoization and missed the cache; on Op::Return the key
    /// is used to write the return value back into the cache.
    /// `None` means "don't memoize" — either the function isn't pure,
    /// the call wasn't through Op::Call, or memoization is disabled.
    memo_key: Option<(u32, [u8; 16])>,
    /// #464 step 2: start index of this frame's records in
    /// `Vm::stack_record_arena`. On `Op::Return`, the arena is
    /// truncated back here. Identical lifetime discipline to
    /// `locals_start`.
    stack_record_arena_start: usize,
    /// Remaining stack-record budget for this frame, in Value-slot
    /// units (#464 step 2). Initial value: `STACK_RECORD_BUDGET_SLOTS`.
    /// When an `Op::AllocStackRecord` would consume more slots than
    /// remain, the VM falls back to the heap path silently (same
    /// observable effect as `Op::MakeRecord`), so the budget never
    /// surfaces as a user-visible error.
    stack_record_budget_remaining: u32,
}

/// Sum of `[budget(N)]` declarations on a function's signature
/// (#225). Used by Op::Call / Op::TailCall / Op::CallClosure to
/// notify the EffectHandler of per-call budget cost so the handler
/// can deduct from a shared pool and refuse calls that would
/// exceed the policy ceiling. Negative `Int` args are ignored —
/// the static check (`policy::check_program`) treats budgets as
/// non-negative.
fn call_budget_cost(f: &crate::program::Function) -> u64 {
    let mut total: u64 = 0;
    for e in &f.effects {
        if e.kind == "budget" {
            if let Some(crate::program::EffectArg::Int(n)) = &e.arg {
                if *n >= 0 {
                    total = total.saturating_add(*n as u64);
                }
            }
        }
    }
    total
}

/// Hash the argument list for a pure-fn memoization lookup (#229).
///
/// The memo cache (`pure_memo`) is keyed on this 128-bit digest with
/// no secondary equality check, so the contract is: argument lists
/// that are equal under `Value`'s `PartialEq` must produce the same
/// digest, and the 128-bit width keeps the false-collision rate
/// (which would return a wrong cached result) negligible.
///
/// History (#461 follow-up): this used to build a `serde_json::Value`
/// of every arg, canonicalize it, and SHA-256 the bytes. Profiling
/// the `response_build` workload showed that path at 27.6% of all
/// instructions — it dominated the VM, since every effect-free call
/// pays it whether or not the cache ever hits. The cache is per-`Vm`
/// and ephemeral, so a cryptographic, cross-process-stable key was
/// never needed. We now walk the `Value` tree directly into two
/// domain-separated `SipHash` passes (deterministic fixed-key
/// `DefaultHasher`), concatenating the two 64-bit outputs into a
/// 128-bit key. No JSON allocation, no crypto.
///
/// The walk mirrors `Value::PartialEq` so the equal-args-equal-key
/// contract holds: `Record` is hashed order-independently over its
/// fields (matching `IndexMap`'s order-insensitive equality),
/// `Closure` on `(body_hash, captures)` not `fn_id` (#222), and
/// `Actor`/`Ticker` on pointer identity (matching `Arc::ptr_eq`).
fn hash_call_args(args: &[Value]) -> [u8; 16] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h0 = DefaultHasher::new();
    let mut h1 = DefaultHasher::new();
    // Domain separator: makes the two passes diverge so the
    // concatenated halves span the full 128-bit space rather than
    // duplicating one 64-bit value.
    h1.write_u8(0x9e);
    h0.write_usize(args.len());
    h1.write_usize(args.len());
    for a in args {
        hash_value_into(a, &mut h0);
        hash_value_into(a, &mut h1);
    }
    let lo = h0.finish();
    let hi = h1.finish();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

/// Structural hash of a `Value` into `h`, consistent with
/// `Value::PartialEq`. The leading discriminant byte keeps distinct
/// variants from colliding (e.g. `Int(0)` vs `Bool(false)`).
fn hash_value_into<H: std::hash::Hasher>(v: &Value, h: &mut H) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher as _;
    match v {
        Value::Int(n) => { h.write_u8(0x01); h.write_i64(*n); }
        // Bit pattern, not value: total and deterministic. NaN==NaN
        // by bits (a memo hit there is harmless — the callee is pure
        // and returns the same result for bit-identical args), and
        // +0.0/-0.0 differ (a harmless extra miss).
        Value::Float(f) => { h.write_u8(0x02); h.write_u64(f.to_bits()); }
        Value::Bool(b) => { h.write_u8(0x03); h.write_u8(*b as u8); }
        Value::Str(s) => {
            h.write_u8(0x04);
            h.write_usize(s.len());
            h.write(s.as_bytes());
        }
        Value::Bytes(b) => {
            h.write_u8(0x05);
            h.write_usize(b.len());
            h.write(b);
        }
        Value::Unit => { h.write_u8(0x06); }
        Value::List(items) => {
            h.write_u8(0x07);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        Value::Tuple(items) => {
            h.write_u8(0x08);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        Value::Deque(items) => {
            h.write_u8(0x09);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        // `IndexMap` equality is order-insensitive, so the hash must
        // be too: combine per-entry sub-hashes with wrapping add (a
        // commutative mix) rather than feeding them in iteration
        // order.
        Value::Record { fields, .. } => {
            h.write_u8(0x0a);
            let mut combined: u64 = 0;
            for (k, val) in fields.iter() {
                let mut e = DefaultHasher::new();
                e.write(k.as_bytes());
                e.write_u8(0xff);
                hash_value_into(val, &mut e);
                combined = combined.wrapping_add(e.finish());
            }
            h.write_u64(combined);
            h.write_usize(fields.len());
        }
        Value::Variant { name, args } => {
            h.write_u8(0x0b);
            h.write_usize(name.len());
            h.write(name.as_bytes());
            h.write_usize(args.len());
            for a in args { hash_value_into(a, h); }
        }
        // Identity is `(body_hash, captures)`, not `fn_id` (#222).
        Value::Closure { body_hash, captures, .. } => {
            h.write_u8(0x0c);
            h.write(body_hash);
            h.write_usize(captures.len());
            for c in captures { hash_value_into(c, h); }
        }
        Value::F64Array { rows, cols, data } => {
            h.write_u8(0x0d);
            h.write_u32(*rows);
            h.write_u32(*cols);
            for f in data { h.write_u64(f.to_bits()); }
        }
        // BTreeMap / BTreeSet iterate in sorted key order — already
        // canonical, so direct feed is order-independent.
        Value::Map(m) => {
            h.write_u8(0x0e);
            h.write_usize(m.len());
            for (k, val) in m {
                hash_mapkey_into(k, h);
                hash_value_into(val, h);
            }
        }
        Value::Set(s) => {
            h.write_u8(0x0f);
            h.write_usize(s.len());
            for k in s { hash_mapkey_into(k, h); }
        }
        // Pointer identity, matching `Arc::ptr_eq` in PartialEq.
        Value::Actor(a) => {
            h.write_u8(0x10);
            h.write_usize(Arc::as_ptr(a) as *const () as usize);
        }
        Value::Ticker(t) => {
            h.write_u8(0x11);
            h.write_usize(Arc::as_ptr(t) as *const () as usize);
        }
        // Coarse summary (schema + dimensions), matching the prior
        // `to_json` encoding which deliberately omitted the cell data
        // (tables can be GB-scale). Equal tables share schema + dims
        // so equal-args-equal-key holds; this is no coarser than the
        // pre-#461-followup behavior.
        Value::ArrowTable(t) => {
            h.write_u8(0x12);
            h.write_i64(t.num_rows() as i64);
            h.write_i64(t.num_columns() as i64);
            for f in t.schema().fields() {
                h.write(f.name().as_bytes());
                h.write_u8(0xfe);
            }
        }
        // #464: a StackRecord crossing into the memo path means an
        // escape the analysis was supposed to reject. Mirror the
        // PartialEq / to_json panic rather than mint a bogus key.
        Value::StackRecord { .. } =>
            panic!("BUG(#464): Value::StackRecord reached memo hashing — \
                    escape analysis should have prevented escape to a call boundary"),
        Value::StackTuple { .. } =>
            panic!("BUG(#464): Value::StackTuple reached memo hashing — \
                    escape analysis should have prevented escape to a call boundary"),
        // #463 slice 2a: arena handles must never reach memo hashing.
        // The memo cache outlives every request scope, so a hashed
        // arena handle would dangle. Slice 1's arena-eligibility
        // analysis must exclude pure-fn allocation sites (the memo
        // path is reached only through pure-fn calls) — any reach
        // here is a soundness bug.
        Value::ArenaRecord { .. } =>
            panic!("BUG(#463): Value::ArenaRecord reached memo hashing — \
                    arena-eligibility analysis must exclude pure-fn allocation sites"),
        Value::ArenaTuple { .. } =>
            panic!("BUG(#463): Value::ArenaTuple reached memo hashing — \
                    arena-eligibility analysis must exclude pure-fn allocation sites"),
    }
}

/// Hash a `MapKey` into `h` with its own discriminant so a `Str`
/// key and an `Int` key never collide.
fn hash_mapkey_into<H: std::hash::Hasher>(k: &crate::value::MapKey, h: &mut H) {
    use crate::value::MapKey;
    match k {
        MapKey::Str(s) => { h.write_u8(0x01); h.write_usize(s.len()); h.write(s.as_bytes()); }
        MapKey::Int(n) => { h.write_u8(0x02); h.write_i64(*n); }
    }
}

/// Evaluate a refinement predicate at runtime against the actual
/// argument value (#209 slice 3). Mirrors `lex_types::discharge`'s
/// static evaluator but operates on `Value` directly.
///
/// Returns `Ok(true)` / `Ok(false)` for a clean boolean verdict, or
/// `Err(reason)` if the predicate references something the runtime
/// can't resolve (free variable beyond the binding, unsupported AST
/// node). Callers map `Ok(false)` and `Err` to `VmError::RefinementFailed`.
fn eval_refinement(
    predicate: &lex_ast::CExpr,
    binding: &str,
    arg: &Value,
) -> Result<bool, String> {
    match eval_refinement_inner(predicate, binding, arg) {
        Ok(Value::Bool(b)) => Ok(b),
        Ok(other) => Err(format!("predicate didn't reduce to a Bool, got {other:?}")),
        Err(e) => Err(e),
    }
}

fn eval_refinement_inner(
    e: &lex_ast::CExpr,
    binding: &str,
    arg: &Value,
) -> Result<Value, String> {
    use lex_ast::{CExpr, CLit};
    match e {
        CExpr::Literal { value } => Ok(match value {
            CLit::Int { value } => Value::Int(*value),
            CLit::Float { value } => Value::Float(value.parse().unwrap_or(0.0)),
            CLit::Bool { value } => Value::Bool(*value),
            CLit::Str { value } => Value::Str(value.as_str().into()),
            CLit::Bytes { value } => Value::Str(value.as_str().into()), // hex; unusual in predicates
            CLit::Unit => Value::Unit,
        }),
        CExpr::Var { name } if name == binding => Ok(arg.clone()),
        CExpr::Var { name } => Err(format!(
            "predicate references free var `{name}`; runtime check \
             only resolves the binding (slice 4 will plumb call-site \
             context)")),
        CExpr::UnaryOp { op, expr } => {
            let v = eval_refinement_inner(expr, binding, arg)?;
            match (op.as_str(), v) {
                ("not", Value::Bool(b)) => Ok(Value::Bool(!b)),
                ("-", Value::Int(n)) => Ok(Value::Int(-n)),
                ("-", Value::Float(n)) => Ok(Value::Float(-n)),
                (o, v) => Err(format!("unsupported unary `{o}` on {v:?}")),
            }
        }
        CExpr::BinOp { op, lhs, rhs } => {
            // Short-circuit `and` / `or` for the same reasons as the
            // static evaluator.
            if op == "and" || op == "or" {
                let l = eval_refinement_inner(lhs, binding, arg)?;
                let lb = match l {
                    Value::Bool(b) => b,
                    other => return Err(format!("`{op}` on non-bool: {other:?}")),
                };
                if op == "and" && !lb { return Ok(Value::Bool(false)); }
                if op == "or"  &&  lb { return Ok(Value::Bool(true));  }
                let r = eval_refinement_inner(rhs, binding, arg)?;
                return match r {
                    Value::Bool(b) => Ok(Value::Bool(b)),
                    other => Err(format!("`{op}` on non-bool: {other:?}")),
                };
            }
            let l = eval_refinement_inner(lhs, binding, arg)?;
            let r = eval_refinement_inner(rhs, binding, arg)?;
            apply_refinement_binop(op, &l, &r)
        }
        // Other AST forms (Call, Let, Match, FieldAccess, Lambda,
        // Block, Constructors, Records, Tuples, Lists, Return) need
        // a more general evaluator that can call back into the VM.
        // Out of scope for slice 3; a future slice may unify this
        // with the spec-checker's gate evaluator.
        other => Err(format!("unsupported predicate node: {other:?}")),
    }
}

fn apply_refinement_binop(op: &str, l: &Value, r: &Value) -> Result<Value, String> {
    use Value::*;
    match (op, l, r) {
        ("+", Int(a), Int(b)) => Ok(Int(a + b)),
        ("-", Int(a), Int(b)) => Ok(Int(a - b)),
        ("*", Int(a), Int(b)) => Ok(Int(a * b)),
        ("/", Int(a), Int(b)) if *b != 0 => Ok(Int(a / b)),
        ("%", Int(a), Int(b)) if *b != 0 => Ok(Int(a % b)),
        ("+", Float(a), Float(b)) => Ok(Float(a + b)),
        ("-", Float(a), Float(b)) => Ok(Float(a - b)),
        ("*", Float(a), Float(b)) => Ok(Float(a * b)),
        ("/", Float(a), Float(b)) => Ok(Float(a / b)),

        ("==", a, b) => Ok(Bool(a == b)),
        ("!=", a, b) => Ok(Bool(a != b)),

        ("<",  Int(a), Int(b)) => Ok(Bool(a < b)),
        ("<=", Int(a), Int(b)) => Ok(Bool(a <= b)),
        (">",  Int(a), Int(b)) => Ok(Bool(a > b)),
        (">=", Int(a), Int(b)) => Ok(Bool(a >= b)),

        ("<",  Float(a), Float(b)) => Ok(Bool(a < b)),
        ("<=", Float(a), Float(b)) => Ok(Bool(a <= b)),
        (">",  Float(a), Float(b)) => Ok(Bool(a > b)),
        (">=", Float(a), Float(b)) => Ok(Bool(a >= b)),

        (op, a, b) => Err(format!(
            "unsupported binop `{op}` on {a:?} and {b:?}")),
    }
}

fn const_str(constants: &[Const], idx: u32) -> String {
    match constants.get(idx as usize) {
        Some(Const::NodeId(s)) | Some(Const::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Read `LEX_PAR_MAX_CONCURRENCY` (default = available CPU cores,
/// fallback 4). Capped at 64 so a malformed env var can't spawn an
/// unreasonable number of OS threads.
/// Order-defining comparator for `list.sort_by` keys (#338).
/// Same-typed Int / Float / Str pairs compare via their native
/// `Ord` / `PartialOrd`. Mixed-type or other key shapes compare
/// as Equal; combined with `Vec::sort_by`'s stability that
/// preserves the original element order — best-effort fallback
/// that never panics.
fn compare_sort_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

fn par_max_concurrency() -> usize {
    let from_env = std::env::var("LEX_PAR_MAX_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0);
    let default = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    from_env.unwrap_or(default).min(64)
}

/// `list.par_map`'s runtime: spawn OS threads (capped by
/// `LEX_PAR_MAX_CONCURRENCY`), apply `closure` to each item, return
/// results in input order. Each worker runs a fresh `Vm` with
/// [`DenyAllEffects`] for #305 slice 1 — effectful closures fail
/// with `VmError::Effect`. Slice 2 will plumb a per-thread effect
/// handler split.
fn par_map_run<'a>(
    program: &'a Program,
    closure: Value,
    items: Vec<Value>,
    worker_handlers: Vec<Box<dyn EffectHandler + Send>>,
) -> Result<Vec<Value>, VmError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let n_workers = worker_handlers.len().min(items.len()).max(1);
    // Carve items into `n_workers` round-robin buckets so each
    // worker processes (indices, items) pairs and we can reassemble
    // in input order.
    let mut buckets: Vec<Vec<(usize, Value)>> = (0..n_workers).map(|_| Vec::new()).collect();
    for (i, v) in items.into_iter().enumerate() {
        buckets[i % n_workers].push((i, v));
    }
    let n_total: usize = buckets.iter().map(|b| b.len()).sum();
    let results: std::sync::Mutex<Vec<Option<Result<Value, String>>>> =
        std::sync::Mutex::new((0..n_total).map(|_| None).collect());

    // Pair each bucket with its pre-built handler so workers own
    // their handler outright — no shared mutable state across
    // worker threads.
    let mut worker_handlers = worker_handlers;
    worker_handlers.truncate(n_workers);
    type Pair = (Vec<(usize, Value)>, Box<dyn EffectHandler + Send>);
    let pairs: Vec<Pair> = buckets.into_iter().zip(worker_handlers).collect();

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(pairs.len());
        for (bucket, handler) in pairs {
            let closure = closure.clone();
            let results = &results;
            handles.push(s.spawn(move || {
                // `Box<dyn EffectHandler + Send>` has implicit
                // `+ 'static`; that coerces to `+ 'a` because
                // `'static` outlives any `'a`. The `Send` bound is
                // auto-erased on the unsize coercion.
                let handler_for_vm: Box<dyn EffectHandler + 'a> = handler;
                let mut vm = Vm::with_handler(program, handler_for_vm);
                for (idx, item) in bucket {
                    let r = vm
                        .invoke_closure_value(closure.clone(), vec![item])
                        .map_err(|e| format!("{e:?}"));
                    results.lock().unwrap()[idx] = Some(r);
                }
            }));
        }
        for h in handles {
            h.join().map_err(|_| ()).ok();
        }
    });

    let mut out = Vec::with_capacity(n_total);
    let inner = results.into_inner().unwrap();
    for r in inner {
        match r {
            Some(Ok(v)) => out.push(v),
            Some(Err(e)) => return Err(VmError::Effect(format!("par_map worker: {e}"))),
            None => return Err(VmError::Panic("par_map worker did not produce a result".into())),
        }
    }
    Ok(out)
}

impl<'a> Vm<'a> {
    pub fn new(program: &'a Program) -> Self {
        Self::with_handler(program, Box::new(DenyAllEffects))
    }

    pub fn with_handler(program: &'a Program, handler: Box<dyn EffectHandler + 'a>) -> Self {
        Self {
            program,
            handler,
            tracer: Box::new(NullTracer),
            // Pre-allocate enough capacity for a typical request so the first
            // call incurs no reallocation (#389 slice 3).
            frames: Vec::with_capacity(32),
            stack: Vec::with_capacity(128),
            step_limit: 10_000_000,
            steps: 0,
            pure_memo: std::collections::HashMap::new(),
            pure_memo_hits: 0,
            pure_memo_misses: 0,
            pure_memo_skips: 0,
            memo_fn_state: vec![MemoFnState::default(); program.functions.len()],
            field_ics: vec![Vec::new(); program.functions.len()],
            // 256 slots handles ~32 frames × 8 locals; grows on demand and
            // retains capacity across consecutive vm.call() invocations.
            locals_storage: Vec::with_capacity(256),
            // #464 step 2: zero capacity at construction — handlers that
            // never AllocStackRecord (most code today, until the lowering
            // pass kicks in) pay nothing. First allocation triggers Vec
            // growth; capacity is retained across `vm.call` invocations.
            stack_record_arena: Vec::new(),
            stack_record_allocs: 0,
            stack_record_heap_fallbacks: 0,
            heap_record_allocs: 0,
            // #463 slice 2a: empty until the first enter_request_scope.
            // Programs that never enter a scope incur zero arena cost
            // (the alloc ops, if reached, fall back to the heap path).
            arena_slab: Vec::new(),
            arena_scope_starts: Vec::new(),
            arena_record_allocs: 0,
            arena_record_heap_fallbacks: 0,
            jit_hook: None,
        }
    }

    pub fn set_tracer(&mut self, tracer: Box<dyn Tracer + 'a>) {
        self.tracer = tracer;
    }

    /// Install (or replace) the JIT hook consulted by `Op::Call`'s
    /// dispatch arm. With `None`, dispatch behaves exactly as before
    /// — the hook check is a single null-option branch the optimizer
    /// can hoist. See the [`crate::jit_hook`] module for the
    /// contract callers must uphold.
    pub fn set_jit_hook(&mut self, hook: Option<Box<dyn crate::jit_hook::JitHook + 'a>>) {
        self.jit_hook = hook;
    }

    /// Cap the number of opcode dispatches before the VM aborts with
    /// `step limit exceeded`. Useful as a runtime DoS guard against
    /// untrusted code (e.g. the `agent-tool` sandbox, where an LLM
    /// could emit `list.fold(list.range(0, 1_000_000_000), …)` to hang
    /// the host). Default is 10_000_000.
    pub fn set_step_limit(&mut self, limit: u64) {
        self.step_limit = limit;
    }

    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let fn_id = self.program.lookup(name).ok_or_else(|| VmError::Panic(format!("no function `{name}`")))?;
        self.invoke(fn_id, args)
    }

    /// Vm-level handler for `parser.run` (#221). Routed here from
    /// `Op::EffectCall` rather than through the `EffectHandler` so
    /// the recursive parser interpreter has reentrant Vm access for
    /// closure invocation. Returns the wrapped `Result[T, ParseErr]`
    /// value the language sees.
    fn run_parser_op(&mut self, args: Vec<Value>) -> Result<Value, String> {
        let parser = args.first().cloned()
            .ok_or_else(|| "parser.run: missing parser arg".to_string())?;
        let input = match args.get(1) {
            Some(Value::Str(s)) => s.clone(),
            _ => return Err("parser.run: input must be Str".into()),
        };
        match crate::parser_runtime::run_parser(&parser, &input, 0, self) {
            Ok((value, _pos)) => Ok(Value::Variant {
                name: "Ok".into(),
                args: vec![value],
            }),
            Err((pos, msg)) => {
                let mut e: IndexMap<String, Value> = IndexMap::new();
                e.insert("pos".into(), Value::Int(pos as i64));
                e.insert("message".into(), Value::Str(msg.into()));
                Ok(Value::Variant {
                    name: "Err".into(),
                    args: vec![Value::record_dynamic(e)],
                })
            }
        }
    }

    // ---- Variant helpers used by conc.* registry ops (#444) ----
    // Local helpers (avoid pulling in serde / public API). Lex's
    // `Result`/`Option` are stdlib unions; their runtime shape is a
    // `Value::Variant { name, args }` with the constructor name as
    // declared (`Ok`/`Err`/`Some`/`None`).

    /// VM-level handler for `conc.*` effect ops (#381).
    ///
    /// * `conc.spawn(init, handler)` — creates an `Actor` wrapping the
    ///   initial state and the handler closure. No background thread is
    ///   started; the actor runs synchronously on the calling thread
    ///   under a `Mutex` so concurrent callers serialise.
    ///
    /// * `conc.ask(actor, msg)` — locks the actor, calls
    ///   `handler(state, msg)` on *this* VM (reentrant), expects a
    ///   2-tuple `(new_state, reply)`, updates the actor's state, and
    ///   returns `reply`.
    ///
    /// * `conc.tell(actor, msg)` — same as `ask` but discards the
    ///   reply and returns `Unit`.
    fn run_conc_op(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "spawn" => {
                let mut it = args.into_iter();
                let init = it.next().unwrap_or(Value::Unit);
                let handler = it.next().unwrap_or(Value::Unit);
                if !matches!(handler, Value::Closure { .. }) {
                    return Err(format!(
                        "conc.spawn: handler must be a Closure, got {handler:?}"));
                }
                Ok(Value::Actor(Arc::new(Mutex::new(ActorCell {
                    state: init,
                    handler: crate::value::ActorHandler::Lex(handler),
                }))))
            }
            "ask" | "tell" => {
                let mut it = args.into_iter();
                let actor_val = it.next().unwrap_or(Value::Unit);
                let msg = it.next().unwrap_or(Value::Unit);
                let cell = match actor_val {
                    Value::Actor(ref arc) => Arc::clone(arc),
                    other => return Err(format!(
                        "conc.{op}: first arg must be an Actor, got {other:?}")),
                };
                // Lock the actor: guarantees at-most-one-concurrent message.
                let mut guard = cell.lock().map_err(|e| format!("conc.{op}: actor mutex poisoned: {e}"))?;
                let handler = guard.handler.clone();
                let state = guard.state.clone();
                match handler {
                    crate::value::ActorHandler::Lex(closure_val) => {
                        // Call handler(state, msg) on this VM — full effect access.
                        let result = self.invoke_closure_value(closure_val, vec![state, msg])
                            .map_err(|e| format!("conc.{op}: handler error: {e:?}"))?;
                        // Expect (new_state, reply) tuple.
                        match result {
                            Value::Tuple(mut parts) if parts.len() == 2 => {
                                let reply = parts.pop().unwrap();
                                let new_state = parts.pop().unwrap();
                                guard.state = new_state;
                                drop(guard);
                                if op == "ask" { Ok(reply) } else { Ok(Value::Unit) }
                            }
                            other => Err(format!(
                                "conc.{op}: handler must return a 2-tuple (new_state, reply), got {other:?}")),
                        }
                    }
                    crate::value::ActorHandler::Native(native) => {
                        // Native bridge: fire-and-forget; `state` is unused
                        // (the bridge's "state" is the external resource, e.g.
                        // a WebSocket connection). The closure receives `msg`
                        // directly. `ask` returns whatever the bridge produces;
                        // `tell` discards it. State stays untouched.
                        drop(guard);
                        let result = (native.send)(msg)
                            .map_err(|e| format!("conc.{op}: native handler error: {e}"))?;
                        if op == "ask" { Ok(result) } else { Ok(Value::Unit) }
                    }
                }
            }
            "register" => {
                // conc.register(actor, name) -> Result[Unit, ConcError]
                // Returns Ok(Unit) on first register, Err(AlreadyRegistered(name))
                // if the name is taken. v1 stores the actor opaquely —
                // see crate::conc_registry for the type-tag note.
                let mut it = args.into_iter();
                let actor = it.next().unwrap_or(Value::Unit);
                if !matches!(actor, Value::Actor(_)) {
                    return Err(format!(
                        "conc.register: first arg must be an Actor, got {actor:?}"));
                }
                let name = match it.next() {
                    Some(Value::Str(s)) => s.to_string(),
                    other => return Err(format!(
                        "conc.register: name must be Str, got {other:?}")),
                };
                Ok(match crate::conc_registry::register(&name, actor) {
                    Ok(()) => variant_ok(Value::Unit),
                    Err(crate::conc_registry::RegError::AlreadyRegistered(n)) => {
                        variant_err(variant("AlreadyRegistered", vec![Value::Str(n.into())]))
                    }
                    Err(crate::conc_registry::RegError::NotRegistered(_)) => {
                        unreachable!("register cannot produce NotRegistered")
                    }
                })
            }
            "lookup" => {
                // conc.lookup(name) -> Option[Actor[S, M]]
                // Returns Some(actor) if registered, None otherwise. The
                // [S, M] static parametrisation at the call site is not
                // checked at runtime in v1 — caller's responsibility to
                // match the registration site's type.
                let mut it = args.into_iter();
                let name = match it.next() {
                    Some(Value::Str(s)) => s.to_string(),
                    other => return Err(format!(
                        "conc.lookup: name must be Str, got {other:?}")),
                };
                Ok(match crate::conc_registry::lookup(&name) {
                    Some(actor) => variant("Some", vec![actor]),
                    None => variant("None", vec![]),
                })
            }
            "unregister" => {
                // conc.unregister(name) -> Result[Unit, ConcError]
                let mut it = args.into_iter();
                let name = match it.next() {
                    Some(Value::Str(s)) => s.to_string(),
                    other => return Err(format!(
                        "conc.unregister: name must be Str, got {other:?}")),
                };
                Ok(match crate::conc_registry::unregister(&name) {
                    Ok(()) => variant_ok(Value::Unit),
                    Err(crate::conc_registry::RegError::NotRegistered(n)) => {
                        variant_err(variant("NotRegistered", vec![Value::Str(n.into())]))
                    }
                    Err(crate::conc_registry::RegError::AlreadyRegistered(_)) => {
                        unreachable!("unregister cannot produce AlreadyRegistered")
                    }
                })
            }
            "registered" => {
                // conc.registered() -> List[Str] — sorted snapshot.
                let names = crate::conc_registry::registered();
                Ok(Value::List(names.into_iter()
                    .map(|n| Value::Str(n.into()))
                    .collect()))
            }
            other => Err(format!("unknown conc.{other}")),
        }
    }

    /// Invoke a `Value::Closure` by combining its captures with the
    /// supplied call args and dispatching to the underlying function.
    /// Used by the parser interpreter (#221) to call user-supplied
    /// `f` arguments inside `parser.map` / `parser.and_then` nodes.
    pub fn invoke_closure_value(
        &mut self,
        closure: Value,
        args: Vec<Value>,
    ) -> Result<Value, VmError> {
        let (fn_id, captures) = match closure {
            Value::Closure { fn_id, captures, .. } => (fn_id, captures),
            other => return Err(VmError::TypeMismatch(
                format!("invoke_closure_value: not a closure: {other:?}"))),
        };
        let mut combined = captures;
        combined.extend(args);
        self.invoke(fn_id, combined)
    }

    /// Invoke a 1-arg closure without allocating a separate args
    /// `Vec` (#464 call-overhead). The closure's own `captures` Vec
    /// is reused as the combined `captures ++ [arg]` argument buffer,
    /// so the per-element call in `ListMap`/`ListFilter`/`SortByKey`
    /// allocates at most once (the `push`) instead of twice (a fresh
    /// `vec![arg]` plus the `extend`). Semantically identical to
    /// `invoke_closure_value(closure, vec![arg])`.
    pub fn invoke_closure_1(&mut self, closure: Value, arg: Value) -> Result<Value, VmError> {
        let (fn_id, mut combined) = match closure {
            Value::Closure { fn_id, captures, .. } => (fn_id, captures),
            other => return Err(VmError::TypeMismatch(
                format!("invoke_closure_1: not a closure: {other:?}"))),
        };
        combined.push(arg);
        self.invoke(fn_id, combined)
    }

    /// Invoke a 2-arg closure without a separate args `Vec` — the
    /// `ListFold` combiner path. See `invoke_closure_1`.
    pub fn invoke_closure_2(&mut self, closure: Value, a: Value, b: Value) -> Result<Value, VmError> {
        let (fn_id, mut combined) = match closure {
            Value::Closure { fn_id, captures, .. } => (fn_id, captures),
            other => return Err(VmError::TypeMismatch(
                format!("invoke_closure_2: not a closure: {other:?}"))),
        };
        combined.push(a);
        combined.push(b);
        self.invoke(fn_id, combined)
    }

    /// Open a request-scoped arena via the underlying
    /// `EffectHandler::enter_request_scope` (#463 scaffolding).
    /// Runtime layers — `net.serve_fn`, `net.serve_ws`,
    /// `net.serve_quic` — call this immediately before invoking the
    /// user handler closure for a single request. Pair with
    /// `exit_request_scope` once the response has been built and
    /// any lazy iterators in it have been drained (#477).
    ///
    /// Returns the scope id the runtime should pass back to
    /// `exit_request_scope`. The handler's default impl returns 0
    /// and the matching `exit` is a no-op; `DefaultHandler`'s
    /// implementation actually allocates an arena.
    pub fn enter_request_scope(&mut self) -> u64 {
        // #463 slice 2a: snapshot the slab high-water mark so
        // `exit_request_scope` can truncate back to here, releasing
        // every arena-allocated value the scope built in O(1).
        self.arena_scope_starts.push(self.arena_slab.len() as u32);
        self.handler.enter_request_scope()
    }

    /// True iff there is at least one active request scope — i.e. an
    /// `enter_request_scope` not yet matched by `exit_request_scope`.
    /// Runtime layers use this to skip `materialize_arena_handles` on
    /// paths where no scope was entered (e.g. tiny-http worker
    /// dispatch), keeping the no-arena path zero-cost. Slice 2b-i.
    pub fn arena_scope_active(&self) -> bool {
        !self.arena_scope_starts.is_empty()
    }

    /// Close the request scope opened by `enter_request_scope`.
    /// Drops the associated arena.
    pub fn exit_request_scope(&mut self, scope_id: u64) {
        // #463 slice 2a: truncate the slab back to the matching
        // `enter` snapshot, then notify the handler. Out-of-order /
        // unpaired exits (e.g. a stray `exit` with no prior `enter`)
        // are tolerated as no-ops — the handler does the same, and a
        // stray exit shouldn't crash a live server.
        if let Some(start) = self.arena_scope_starts.pop() {
            self.arena_slab.truncate(start as usize);
        }
        self.handler.exit_request_scope(scope_id)
    }

    /// Deep-walk `value` and resolve every `Value::ArenaRecord` /
    /// `Value::ArenaTuple` handle into its heap-owned equivalent
    /// (`Value::Record` / `Value::Tuple`), reading field contents
    /// out of `Vm::arena_slab` along the way. Primitives, closures,
    /// maps/sets, and the host-managed handles (`Actor` / `Ticker` /
    /// `ArrowTable`) are returned unchanged.
    ///
    /// **The boundary helper** flagged in
    /// `docs/design/arena-plumbing.md` § "Arena handles MUST be
    /// readable at serialization". Callers — the response
    /// serialization path in `lex-runtime`, the trace recorder when
    /// it records a Call/EffectCall arg, anywhere a value crosses
    /// out of the VM into host-managed storage — call this
    /// **while the producing scope is still active**, before
    /// `exit_request_scope`. After exit the slab is truncated, so a
    /// handle materialized after-the-fact would read garbage (or
    /// panic on the bounds check).
    ///
    /// `Value::StackRecord` / `Value::StackTuple` would similarly
    /// need slab resolution, but the #464 escape analysis prevents
    /// them from reaching boundary-crossing ops in the first place
    /// (they're frame-local by construction). Reaching here means a
    /// hand-built or analysis-buggy program; we panic with the same
    /// loud-not-silent contract the other inspection paths use.
    ///
    /// Idempotent on already-materialized values (no arena handles
    /// in the tree → only the recursive walk's clones, no slab
    /// lookups). Cost per call is one walk + clone of the tree —
    /// amortized over the per-node mallocs avoided during request
    /// handling, the net stays strongly positive.
    pub fn materialize_arena_handles(&self, value: Value) -> Value {
        use crate::value::Value as V;
        match value {
            // Primitives + opaque handles cross unchanged. Cheap
            // — clones are essentially free for the Copy-ish ones
            // and Arc-bumps for the handle types.
            V::Int(_) | V::Float(_) | V::Bool(_) | V::Str(_) | V::Bytes(_)
            | V::Unit | V::Closure { .. } | V::F64Array { .. }
            | V::Map(_) | V::Set(_) | V::Actor(_) | V::Ticker(_)
            | V::ArrowTable(_) => value,

            // Containers: recurse on each element. Map/Set keys are
            // MapKey (Str | Int), never Value, so no handles can
            // hide there.
            V::List(items) => V::List(
                items.into_iter().map(|v| self.materialize_arena_handles(v)).collect()),
            V::Tuple(items) => V::Tuple(
                items.into_iter().map(|v| self.materialize_arena_handles(v)).collect()),
            V::Deque(items) => V::Deque(
                items.into_iter().map(|v| self.materialize_arena_handles(v)).collect()),
            V::Variant { name, args } => V::Variant {
                name,
                args: args.into_iter().map(|v| self.materialize_arena_handles(v)).collect(),
            },
            V::Record { shape_id, fields } => {
                let mut out: IndexMap<SmolStr, Value> = IndexMap::with_capacity(fields.len());
                for (k, v) in fields.into_iter() {
                    out.insert(k, self.materialize_arena_handles(v));
                }
                V::Record { shape_id, fields: Box::new(out) }
            }

            // The actual resolution work — read the slab and build a
            // heap form. Field-name ordering for ArenaRecord matches
            // the shape's, same as `MakeRecord`'s IndexMap insertion
            // pattern; that's the contract that makes the polymorphic
            // GetField IC work, and we reuse it here.
            V::ArenaRecord { shape_id, slab_start, field_count } => {
                let start = slab_start as usize;
                let n = field_count as usize;
                debug_assert!(start + n <= self.arena_slab.len(),
                    "ArenaRecord handle out of bounds — likely materialized after exit_request_scope");
                let shape = &self.program.record_shapes[shape_id as usize];
                let mut fields: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                for (i, name_const_idx) in shape.iter().take(n).enumerate() {
                    let name: SmolStr = match &self.program.constants[*name_const_idx as usize] {
                        Const::FieldName(s) => s.as_str().into(),
                        _ => panic!("BUG(#463): ArenaRecord shape entry not a FieldName const"),
                    };
                    let v = self.materialize_arena_handles(self.arena_slab[start + i].clone());
                    fields.insert(name, v);
                }
                V::Record { shape_id, fields: Box::new(fields) }
            }
            V::ArenaTuple { slab_start, arity } => {
                let start = slab_start as usize;
                let n = arity as usize;
                debug_assert!(start + n <= self.arena_slab.len(),
                    "ArenaTuple handle out of bounds — likely materialized after exit_request_scope");
                let items: Vec<Value> = (0..n)
                    .map(|i| self.materialize_arena_handles(self.arena_slab[start + i].clone()))
                    .collect();
                V::Tuple(items)
            }

            // #464 stack handles are frame-local; the analysis
            // prevents them from reaching any boundary the
            // materializer is called at. Reach = bug; panic loud.
            V::StackRecord { .. } =>
                panic!("BUG(#464/#463): Value::StackRecord reached materialize_arena_handles \
                        — escape analysis should keep stack handles inside their frame"),
            V::StackTuple { .. } =>
                panic!("BUG(#464/#463): Value::StackTuple reached materialize_arena_handles \
                        — escape analysis should keep stack handles inside their frame"),
        }
    }

    /// Read a named field out of a record without materializing its
    /// parent. Works uniformly on `Value::Record` (heap) and
    /// `Value::ArenaRecord` (slab handle), so a runtime layer can
    /// consume the response record structurally — straight out of
    /// the arena slab — instead of paying for a tree-wide
    /// `materialize_arena_handles` walk just to read three top-level
    /// fields.
    ///
    /// Returns `None` if the value isn't a record or the field
    /// doesn't exist. The returned `Value` is a clone of the slot
    /// contents (records' field values can themselves be records,
    /// variants, etc.; cloning at the boundary is unavoidable
    /// without lifetime trickery on the public API).
    ///
    /// Performance: on the heap path it's a `IndexMap::get` + clone.
    /// On the arena path it's a linear walk of the shape's
    /// field-name vec (`field_count` long, typically ≤ 10) +
    /// an O(1) slab index + clone. The polymorphic-IC equivalent
    /// inside the VM is faster, but this API is for **host**
    /// consumers, not hot-loop dispatch.
    ///
    /// `Value::StackRecord` is deliberately not handled — those
    /// handles are frame-local by construction (#464 escape pass)
    /// and shouldn't reach host boundaries; reaching them here is
    /// a soundness bug surfaced as a panic, matching the existing
    /// inspection-path contract.
    pub fn get_record_field(&self, value: &Value, name: &str) -> Option<Value> {
        match value {
            Value::Record { fields, .. } => fields.get(name).cloned(),
            Value::ArenaRecord { shape_id, slab_start, field_count } => {
                let shape = self.program.record_shapes.get(*shape_id as usize)?;
                let n = (*field_count as usize).min(shape.len());
                for (i, &name_const_idx) in shape.iter().take(n).enumerate() {
                    if let Const::FieldName(s) = &self.program.constants[name_const_idx as usize] {
                        if s == name {
                            return Some(self.arena_slab[*slab_start as usize + i].clone());
                        }
                    }
                }
                None
            }
            Value::StackRecord { .. } =>
                panic!("BUG(#464): Value::StackRecord reached Vm::get_record_field \
                        — frame-local handles should never reach the host boundary"),
            _ => None,
        }
    }

    /// Positional read out of a tuple without materializing its
    /// parent. Works uniformly on `Value::Tuple` and
    /// `Value::ArenaTuple`. See `get_record_field` for the lifetime
    /// rationale.
    pub fn get_tuple_elem(&self, value: &Value, idx: u16) -> Option<Value> {
        match value {
            Value::Tuple(items) => items.get(idx as usize).cloned(),
            Value::ArenaTuple { slab_start, arity } => {
                if idx >= *arity { return None; }
                Some(self.arena_slab[*slab_start as usize + idx as usize].clone())
            }
            Value::StackTuple { .. } =>
                panic!("BUG(#464): Value::StackTuple reached Vm::get_tuple_elem \
                        — frame-local handles should never reach the host boundary"),
            _ => None,
        }
    }

    /// Arena-aware `to_json` — produces a `serde_json::Value` from
    /// a `Value` whose tree may contain `ArenaRecord` / `ArenaTuple`
    /// handles, reading them straight out of `Vm::arena_slab`
    /// instead of materializing into a heap `Value::Record` mirror
    /// first.
    ///
    /// Equivalent output to `value.to_json()` on a fully-materialized
    /// tree (idempotent in that sense). Use this when serializing a
    /// handler return value to JSON for the response — saves the
    /// per-node IndexMap allocations the materialize-then-to_json
    /// pattern pays.
    pub fn value_to_json(&self, value: &Value) -> serde_json::Value {
        use serde_json::Value as J;
        match value {
            // Primitives + opaque host handles: delegate to the
            // existing `Value::to_json` — its output is identical
            // and it handles the host-handle types we don't model
            // (Actor / Ticker / ArrowTable / F64Array / Map / Set /
            // Closure / Bytes encoding) in one place.
            Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_)
            | Value::Bytes(_) | Value::Unit | Value::Closure { .. }
            | Value::F64Array { .. } | Value::Map(_) | Value::Set(_)
            | Value::Actor(_) | Value::Ticker(_) | Value::ArrowTable(_)
                => value.to_json(),

            Value::List(items) => J::Array(items.iter().map(|v| self.value_to_json(v)).collect()),
            Value::Tuple(items) => J::Array(items.iter().map(|v| self.value_to_json(v)).collect()),
            Value::Deque(items) => J::Array(items.iter().map(|v| self.value_to_json(v)).collect()),
            Value::Variant { name, args } => {
                let mut m = serde_json::Map::new();
                m.insert("$variant".into(), J::String(name.clone()));
                m.insert("args".into(),
                    J::Array(args.iter().map(|v| self.value_to_json(v)).collect()));
                J::Object(m)
            }
            Value::Record { fields, .. } => {
                let mut m = serde_json::Map::new();
                for (k, v) in fields.iter() {
                    m.insert(k.to_string(), self.value_to_json(v));
                }
                J::Object(m)
            }

            // Slab-direct: read the cells in shape order, emit a
            // JSON object using the shape's field names. The cost
            // delta vs the `Value::to_json` materialize-then-walk
            // path is the saved `Box<IndexMap>` allocation +
            // insertion + drop.
            Value::ArenaRecord { shape_id, slab_start, field_count } => {
                let shape = match self.program.record_shapes.get(*shape_id as usize) {
                    Some(s) => s,
                    None => return J::Null,
                };
                let n = (*field_count as usize).min(shape.len());
                let mut m = serde_json::Map::with_capacity(n);
                for (i, &name_const_idx) in shape.iter().take(n).enumerate() {
                    let name = match &self.program.constants[name_const_idx as usize] {
                        Const::FieldName(s) => s.to_string(),
                        _ => continue,
                    };
                    let cell = &self.arena_slab[*slab_start as usize + i];
                    m.insert(name, self.value_to_json(cell));
                }
                J::Object(m)
            }
            Value::ArenaTuple { slab_start, arity } => {
                let start = *slab_start as usize;
                let n = *arity as usize;
                let items: Vec<serde_json::Value> = (0..n)
                    .map(|i| self.value_to_json(&self.arena_slab[start + i]))
                    .collect();
                J::Array(items)
            }

            // Stack handles must not reach the host — same defensive
            // panic as the other inspection paths.
            Value::StackRecord { .. } =>
                panic!("BUG(#464): Value::StackRecord reached Vm::value_to_json \
                        — frame-local handles should never reach the host boundary"),
            Value::StackTuple { .. } =>
                panic!("BUG(#464): Value::StackTuple reached Vm::value_to_json \
                        — frame-local handles should never reach the host boundary"),
        }
    }

    pub fn invoke(&mut self, fn_id: u32, args: Vec<Value>) -> Result<Value, VmError> {
        let f = &self.program.functions[fn_id as usize];
        if args.len() != f.arity as usize {
            return Err(VmError::Panic(format!("arity mismatch calling {}", f.name)));
        }
        // Refinement runtime check at the public entry point too
        // (#209 slice 3). `Op::Call` checks for in-program calls;
        // this branch covers `vm.call("entry", ...)` from the host
        // and the reentrant `invoke_closure_value` path. Same
        // semantics, same error shape.
        //
        // Iterate `f.refinements` by reference — the loop body
        // only reads from `self.program` (via `r`) and from locals,
        // so we don't need to clone the Vec to detach it from
        // `&self`. The function name is cloned **lazily**, only on
        // the failure path: functions with no refinements (the common
        // case) never enter the loop, so the per-call `f.name.clone()`
        // was pure waste on the hot path (#464 call-overhead).
        for (i, refinement) in f.refinements.iter().enumerate() {
            if let Some(r) = refinement {
                let arg = args.get(i).cloned().unwrap_or(Value::Unit);
                match eval_refinement(&r.predicate, &r.binding, &arg) {
                    Ok(true) => {}
                    Ok(false) => return Err(VmError::RefinementFailed {
                        fn_name: f.name.clone(),
                        param_index: i,
                        binding: r.binding.clone(),
                        reason: format!("predicate failed for {} = {arg:?}", r.binding),
                    }),
                    Err(reason) => return Err(VmError::RefinementFailed {
                        fn_name: f.name.clone(),
                        param_index: i,
                        binding: r.binding.clone(),
                        reason,
                    }),
                }
            }
        }
        // #465 JIT tier hook at the public entry — same contract as
        // the `Op::Call` dispatch arm. Pure-fn memo is not consulted
        // at this layer (memo is per-Op::Call); the hook fires
        // unconditionally for refinement-clean calls.
        if let Some(mut hook) = self.jit_hook.take() {
            let hook_result = hook.try_call(fn_id, &args);
            self.jit_hook = Some(hook);
            if let Some(result) = hook_result? {
                return Ok(result);
            }
        }
        let f = &self.program.functions[fn_id as usize];
        // Claim slots from the locals stack allocator (#389 slice 3).
        let locals_start = self.locals_storage.len();
        let locals_len = f.locals_count.max(f.arity) as usize;
        self.locals_storage.resize(locals_start + locals_len, Value::Unit);
        for (i, v) in args.into_iter().enumerate() {
            self.locals_storage[locals_start + i] = v;
        }
        // Record the depth before pushing — this is what `run` will
        // exit at, supporting reentrant invocation from inside the
        // VM (e.g. the parser interpreter calling closures, #221).
        let base_depth = self.frames.len();
        self.push_frame(Frame {
            fn_id, pc: 0, locals_start, locals_len,
            stack_base: self.stack.len(),
            trace_kind: FrameKind::Entry,
            memo_key: None,
            stack_record_arena_start: self.stack_record_arena.len(),
            stack_record_budget_remaining: STACK_RECORD_BUDGET_SLOTS,
        })?;
        self.run_to(base_depth)
    }

    /// All call-frame pushes funnel through here so the depth
    /// check can't be skipped by a missing branch. Returns
    /// `CallStackOverflow` instead of letting recursion blow the
    /// host's native stack.
    fn push_frame(&mut self, frame: Frame) -> Result<(), VmError> {
        if self.frames.len() as u32 >= MAX_CALL_DEPTH {
            return Err(VmError::CallStackOverflow(MAX_CALL_DEPTH));
        }
        self.frames.push(frame);
        Ok(())
    }

    /// Run until the frame stack drops to `base_depth`. Required for
    /// reentrant invocation: a `Vm::invoke` call from inside an
    /// already-running `run()` must return when *its* frame returns,
    /// not when the entire frame stack empties (#221).
    fn run_to(&mut self, base_depth: usize) -> Result<Value, VmError> {
        // #461 slice A: cache the executing function's code slice across
        // ops instead of re-deriving `program.functions[fn_id].code` on
        // every iteration. The program is borrowed (`&'a Program`) and is
        // never mutated during a run, so the slice reference is valid for
        // the whole run and — crucially — is independent of the `&mut self`
        // borrow the op handlers take: it points into the caller-owned
        // `Program`, not into `*self`. Re-resolve only when `fn_id`
        // changes, which is exactly the frame-transition set (Call /
        // CallClosure / TailCall / Return); recursion into the same
        // `fn_id` correctly keeps the cached slice. `frame_idx` / `fn_id`
        // stay recomputed per op (cheap field reads), so the op handlers
        // are untouched and their `fn_id` bindings shadow as before.
        let program: &'a Program = self.program;
        let mut code: &'a [Op] = &[];
        let mut code_fn_id: u32 = u32::MAX;
        loop {
            if self.steps > self.step_limit {
                let frame_idx = self.frames.len() - 1;
                let fn_id = self.frames[frame_idx].fn_id;
                let fn_name = &program.functions[fn_id as usize].name;
                return Err(VmError::Panic(format!(
                    "step limit exceeded in `{fn_name}` ({} > {})",
                    self.steps, self.step_limit,
                )));
            }
            self.steps += 1;
            let frame_idx = self.frames.len() - 1;
            let pc = self.frames[frame_idx].pc;
            let fn_id = self.frames[frame_idx].fn_id;
            if fn_id != code_fn_id {
                code = &program.functions[fn_id as usize].code;
                code_fn_id = fn_id;
            }
            // #461 slice B: the bytecode verifier (#366) proves pc stays
            // in bounds for every reachable op — every path through a
            // function ends in Return / Jump / TailCall, so execution
            // never falls off the end of `code`. The per-op
            // `pc >= code.len()` guard is therefore redundant for verified
            // programs; demote it to a debug-only assertion. The `code[pc]`
            // index below stays bounds-checked, so a malformed program in
            // a release build still panics (loudly, just without the
            // bespoke message) rather than reading out of bounds — no
            // `unsafe`, no UB, only the cold error-return path leaves the
            // hot loop.
            debug_assert!(
                pc < code.len(),
                "ran past end of code in `{}`",
                program.functions[fn_id as usize].name,
            );
            let op = code[pc];
            self.frames[frame_idx].pc = pc + 1;

            match op {
                Op::PushConst(i) => {
                    let c = &self.program.constants[i as usize];
                    self.stack.push(const_to_value(c));
                }
                Op::Pop => { self.pop()?; }
                Op::Dup => {
                    let v = self.peek()?.clone();
                    self.stack.push(v);
                }
                Op::LoadLocal(i) => {
                    let base = self.frames[frame_idx].locals_start;
                    let v = self.locals_storage[base + i as usize].clone();
                    self.stack.push(v);
                }
                Op::StoreLocal(i) => {
                    let v = self.pop()?;
                    let base = self.frames[frame_idx].locals_start;
                    self.locals_storage[base + i as usize] = v;
                }
                Op::MakeRecord { shape_idx, field_count } => {
                    self.heap_record_allocs += 1;
                    let shape = &self.program.record_shapes[shape_idx as usize];
                    let n = field_count as usize;
                    debug_assert_eq!(shape.len(), n,
                        "MakeRecord field_count must match record_shapes[shape_idx].len()");
                    let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n).rev() {
                        values[i] = self.pop()?;
                    }
                    let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                    for (i, val) in values.into_iter().enumerate() {
                        let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                            Const::FieldName(s) => s.as_str().into(),
                            _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                        };
                        rec.insert(name, val);
                    }
                    self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                }
                Op::AllocStackRecord { shape_idx, field_count } => {
                    // #464 step 2. Same value-stack contract as
                    // MakeRecord (pop `field_count`, push 1), but the
                    // fields live in the VM's stack-record arena
                    // instead of a heap-allocated IndexMap.
                    //
                    // Budget check: if this frame's remaining
                    // allocation budget can't cover `field_count`
                    // slots, fall back to MakeRecord behavior. The
                    // observable result is identical (a record
                    // value) so the compiler doesn't need to know
                    // ahead of time whether the budget will hold.
                    let n = field_count as usize;
                    let frame = &mut self.frames[frame_idx];
                    if frame.stack_record_budget_remaining < field_count as u32 {
                        self.stack_record_heap_fallbacks += 1;
                        // Heap fallback path — exact copy of
                        // MakeRecord's body. Compiler emitted
                        // AllocStackRecord because escape analysis
                        // proved the record can stay frame-local;
                        // the budget exhaustion is a runtime cost
                        // ceiling, not a correctness issue.
                        let shape = &self.program.record_shapes[shape_idx as usize];
                        debug_assert_eq!(shape.len(), n,
                            "AllocStackRecord field_count must match record_shapes[shape_idx].len()");
                        let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() {
                            values[i] = self.pop()?;
                        }
                        let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                        for (i, val) in values.into_iter().enumerate() {
                            let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                                Const::FieldName(s) => s.as_str().into(),
                                _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                            };
                            rec.insert(name, val);
                        }
                        self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                    } else {
                        self.stack_record_allocs += 1;
                        // Stack path: append the popped field values
                        // to the arena in shape order (matches the
                        // IndexMap insertion order used by MakeRecord,
                        // so the polymorphic GetField IC sees the same
                        // offset for either variant).
                        frame.stack_record_budget_remaining -= field_count as u32;
                        let slab_start = self.stack_record_arena.len();
                        // Reserve all slots upfront so we can write in
                        // shape order while popping in reverse —
                        // matches MakeRecord's idiom.
                        self.stack_record_arena.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.stack_record_arena[slab_start + i] = v;
                        }
                        self.stack.push(Value::StackRecord {
                            shape_id: shape_idx,
                            slab_start: slab_start as u32,
                            field_count,
                        });
                    }
                }
                Op::AllocArenaRecord { shape_idx, field_count } => {
                    // #463 slice 2a. Same value-stack contract as
                    // MakeRecord, but field values land in the
                    // request-scoped `arena_slab` instead of a
                    // per-field heap IndexMap. Runtime fallback when
                    // no scope is active — the op silently degrades
                    // to the MakeRecord heap path so arena-lowered
                    // bytecode stays sound in non-handler contexts
                    // (REPL, tests, top-level scripts).
                    let n = field_count as usize;
                    if self.arena_scope_starts.is_empty() {
                        self.arena_record_heap_fallbacks += 1;
                        // Heap fallback path — exact copy of
                        // MakeRecord's body. Same compile-time
                        // contract (shape order, IndexMap insertion)
                        // so the resulting Value::Record is
                        // indistinguishable from a direct MakeRecord.
                        let shape = &self.program.record_shapes[shape_idx as usize];
                        debug_assert_eq!(shape.len(), n,
                            "AllocArenaRecord field_count must match record_shapes[shape_idx].len()");
                        let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() {
                            values[i] = self.pop()?;
                        }
                        let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                        for (i, val) in values.into_iter().enumerate() {
                            let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                                Const::FieldName(s) => s.as_str().into(),
                                _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                            };
                            rec.insert(name, val);
                        }
                        self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                    } else {
                        self.arena_record_allocs += 1;
                        // Arena path: append the popped field values
                        // to the slab in shape order (matches
                        // MakeRecord's IndexMap insertion order, so
                        // the polymorphic GetField IC sees the same
                        // offset across all three variants).
                        let slab_start = self.arena_slab.len();
                        self.arena_slab.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.arena_slab[slab_start + i] = v;
                        }
                        self.stack.push(Value::ArenaRecord {
                            shape_id: shape_idx,
                            slab_start: slab_start as u32,
                            field_count,
                        });
                    }
                }
                Op::MakeTuple(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::Tuple(items));
                }
                Op::AllocStackTuple { arity } => {
                    // #464 tuple codegen. Same value-stack contract as
                    // MakeTuple (pop `arity`, push 1), but the elements
                    // live in the shared stack-record arena instead of
                    // a heap Vec. Budget exhaustion falls back to the
                    // MakeTuple heap path — identical observable result.
                    let n = arity as usize;
                    let frame = &mut self.frames[frame_idx];
                    if frame.stack_record_budget_remaining < arity as u32 {
                        self.stack_record_heap_fallbacks += 1;
                        let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() { items[i] = self.pop()?; }
                        self.stack.push(Value::Tuple(items));
                    } else {
                        self.stack_record_allocs += 1;
                        frame.stack_record_budget_remaining -= arity as u32;
                        let slab_start = self.stack_record_arena.len();
                        self.stack_record_arena.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.stack_record_arena[slab_start + i] = v;
                        }
                        self.stack.push(Value::StackTuple {
                            slab_start: slab_start as u32,
                            arity,
                        });
                    }
                }
                Op::AllocArenaTuple { arity } => {
                    // #463 slice 2a. Tuple analogue of
                    // AllocArenaRecord: arena slab when a scope is
                    // active, MakeTuple heap fallback otherwise.
                    let n = arity as usize;
                    if self.arena_scope_starts.is_empty() {
                        self.arena_record_heap_fallbacks += 1;
                        let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() { items[i] = self.pop()?; }
                        self.stack.push(Value::Tuple(items));
                    } else {
                        self.arena_record_allocs += 1;
                        let slab_start = self.arena_slab.len();
                        self.arena_slab.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.arena_slab[slab_start + i] = v;
                        }
                        self.stack.push(Value::ArenaTuple {
                            slab_start: slab_start as u32,
                            arity,
                        });
                    }
                }
                Op::MakeList(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::List(items.into()));
                }
                Op::MakeVariant { name_idx, arity } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let name = match &self.program.constants[name_idx as usize] {
                        Const::VariantName(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected VariantName const".into())),
                    };
                    self.stack.push(Value::Variant { name, args });
                }
                Op::GetField { name_idx, site_idx } => {
                    let v = self.pop()?;
                    match v {
                        Value::Record { fields: r, shape_id } => {
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            // Inline cache keyed by (fn_id, site_idx) with
                            // shape_id-keyed verification (#462). Slot stores
                            // (shape_id_at_install, offset). Hit verification:
                            // - real shape_id (!= NO_SHAPE_ID) matches: offset
                            //   is guaranteed valid (records with the same
                            //   shape_id share the same field-name ordering
                            //   from the compile-time `record_shapes` table).
                            //   One u32 compare; no string work.
                            // - NO_SHAPE_ID matches NO_SHAPE_ID: distinct
                            //   dynamic shapes both carry this sentinel and
                            //   would otherwise alias, so we fall back to
                            //   verifying via name compare against the field
                            //   at the cached offset — the pre-slice
                            //   correctness path.
                            // On any mismatch we walk by name and reinstall
                            // (shape_id, offset).
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id {
                                        if shape_id != crate::value::NO_SHAPE_ID {
                                            // Real shape match: offset is sound.
                                            if let Some((_, val)) = r.get_index(off) {
                                                break 'ic val.clone();
                                            }
                                        } else if let Some((k, val)) = r.get_index(off) {
                                            // Dynamic shape: verify by name.
                                            if let Const::FieldName(s) =
                                                &self.program.constants[name_idx as usize]
                                            {
                                                if s == k { break 'ic val.clone(); }
                                            }
                                        }
                                    }
                                }
                                // Cache miss: resolve by name, install
                                // (shape_id, offset).
                                let name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let (off, _, val) = r.get_full(name)
                                    .ok_or_else(|| VmError::TypeMismatch(
                                        format!("missing field `{name}`")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                val.clone()
                            };
                            self.stack.push(value);
                        }
                        Value::StackRecord { shape_id, slab_start, field_count } => {
                            // #464 step 2: dispatch over a stack-allocated
                            // record. The IC slot stored
                            // (shape_id, offset_in_shape) is interoperable
                            // with the heap path because MakeRecord builds
                            // the IndexMap in shape order — offset N means
                            // the same field in either representation. So
                            // we share `field_ics` with the heap path; no
                            // per-variant cache pollution.
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id && (off as u16) < field_count {
                                        // Shape-keyed verification is sound
                                        // here for the same reason as the
                                        // heap path — compile-time shape
                                        // IDs are issued by
                                        // `Program::record_shapes` and
                                        // their field order is fixed.
                                        // Stack records always carry a
                                        // compile-time shape_id (NO_SHAPE_ID
                                        // is impossible — AllocStackRecord
                                        // is only emitted at compile time
                                        // with a known shape_idx).
                                        let idx = slab_start as usize + off;
                                        break 'ic self.stack_record_arena[idx].clone();
                                    }
                                }
                                // Cache miss: walk the shape's field-name
                                // vec to find the slot for `name_idx`. The
                                // miss path is O(field_count) like the
                                // heap path, but the hot retrieval after
                                // install is one array index — cheaper
                                // than IndexMap::get_index.
                                let shape =
                                    &self.program.record_shapes[shape_id as usize];
                                let target_name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let mut found: Option<usize> = None;
                                for (i, fn_const_idx) in shape.iter().enumerate() {
                                    if let Const::FieldName(s) =
                                        &self.program.constants[*fn_const_idx as usize]
                                    {
                                        if s == target_name { found = Some(i); break; }
                                    }
                                }
                                let off = found.ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{target_name}` on stack record")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                self.stack_record_arena[slab_start as usize + off].clone()
                            };
                            self.stack.push(value);
                        }
                        Value::ArenaRecord { shape_id, slab_start, field_count } => {
                            // #463 slice 2a: dispatch over an
                            // arena-allocated record. Identical IC
                            // story to `StackRecord` above — the slot
                            // stores `(shape_id, offset)` and offset
                            // semantics match `Value::Record`'s
                            // IndexMap insertion order, so the IC is
                            // three-way interoperable.
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id && (off as u16) < field_count {
                                        let idx = slab_start as usize + off;
                                        break 'ic self.arena_slab[idx].clone();
                                    }
                                }
                                let shape =
                                    &self.program.record_shapes[shape_id as usize];
                                let target_name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let mut found: Option<usize> = None;
                                for (i, fn_const_idx) in shape.iter().enumerate() {
                                    if let Const::FieldName(s) =
                                        &self.program.constants[*fn_const_idx as usize]
                                    {
                                        if s == target_name { found = Some(i); break; }
                                    }
                                }
                                let off = found.ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{target_name}` on arena record")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                self.arena_slab[slab_start as usize + off].clone()
                            };
                            self.stack.push(value);
                        }
                        other => return Err(VmError::TypeMismatch(
                            format!("GetField on non-record: {other:?}"))),
                    }
                }
                Op::GetElem(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Tuple(items) => {
                            let v = items.into_iter().nth(i as usize)
                                .ok_or_else(|| VmError::TypeMismatch(format!("tuple index {i} out of range")))?;
                            self.stack.push(v);
                        }
                        // #464 tuple codegen: positional read out of a
                        // frame-local tuple. The arena slot is an O(1)
                        // index, mirroring the heap path's nth().
                        Value::StackTuple { slab_start, arity } => {
                            if i >= arity {
                                return Err(VmError::TypeMismatch(
                                    format!("tuple index {i} out of range")));
                            }
                            let v = self.stack_record_arena[slab_start as usize + i as usize].clone();
                            self.stack.push(v);
                        }
                        // #463 slice 2a: positional read out of an
                        // arena tuple — same O(1) index pattern as
                        // StackTuple but through `arena_slab`.
                        Value::ArenaTuple { slab_start, arity } => {
                            if i >= arity {
                                return Err(VmError::TypeMismatch(
                                    format!("tuple index {i} out of range")));
                            }
                            let v = self.arena_slab[slab_start as usize + i as usize].clone();
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetElem on non-tuple: {other:?}"))),
                    }
                }
                Op::TestVariant(i) => {
                    let name = match &self.program.constants[i as usize] {
                        Const::VariantName(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected VariantName const".into())),
                    };
                    let v = self.pop()?;
                    match &v {
                        Value::Variant { name: vname, .. } => {
                            self.stack.push(Value::Bool(vname == &name));
                        }
                        // For tag-only enums of primitive type (e.g. ParseError = Empty | NotNumber)
                        // the value is currently a Variant too, since constructors emit MakeVariant.
                        other => return Err(VmError::TypeMismatch(format!("TestVariant on non-variant: {other:?}"))),
                    }
                }
                Op::GetVariant(_i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Variant { args, .. } => {
                            self.stack.push(Value::Tuple(args));
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetVariant on non-variant: {other:?}"))),
                    }
                }
                Op::GetVariantArg(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Variant { mut args, .. } => {
                            if (i as usize) >= args.len() {
                                return Err(VmError::TypeMismatch("variant arg index oob".into()));
                            }
                            self.stack.push(args.swap_remove(i as usize));
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetVariantArg on non-variant: {other:?}"))),
                    }
                }
                Op::GetListLen => {
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => self.stack.push(Value::Int(items.len() as i64)),
                        other => return Err(VmError::TypeMismatch(format!("GetListLen on non-list: {other:?}"))),
                    }
                }
                Op::GetListElem(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => {
                            let v = items.into_iter().nth(i as usize)
                                .ok_or_else(|| VmError::TypeMismatch("list index oob".into()))?;
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetListElem on non-list: {other:?}"))),
                    }
                }
                Op::GetListElemDyn => {
                    // Stack: [list, idx]
                    let idx = match self.pop()? {
                        Value::Int(n) => n as usize,
                        other => return Err(VmError::TypeMismatch(format!("GetListElemDyn idx: {other:?}"))),
                    };
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => {
                            let v = items.into_iter().nth(idx)
                                .ok_or_else(|| VmError::TypeMismatch("list index oob".into()))?;
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetListElemDyn on non-list: {other:?}"))),
                    }
                }
                Op::ListAppend => {
                    let value = self.pop()?;
                    let list = self.pop()?;
                    match list {
                        Value::List(mut items) => {
                            items.push_back(value);
                            self.stack.push(Value::List(items));
                        }
                        other => return Err(VmError::TypeMismatch(format!("ListAppend on non-list: {other:?}"))),
                    }
                }
                Op::Jump(off) => {
                    let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                    self.frames[frame_idx].pc = new_pc;
                }
                Op::JumpIf(off) => {
                    let v = self.pop()?;
                    if v.as_bool() {
                        let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                        self.frames[frame_idx].pc = new_pc;
                    }
                }
                Op::JumpIfNot(off) => {
                    let v = self.pop()?;
                    if !v.as_bool() {
                        let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                        self.frames[frame_idx].pc = new_pc;
                    }
                }
                Op::MakeClosure { fn_id, capture_count } => {
                    let n = capture_count as usize;
                    let mut captures: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n).rev() { captures[i] = self.pop()?; }
                    // Look up the canonical body hash so the resulting
                    // `Value::Closure` carries it for equality (#222).
                    let body_hash = self.program.functions[fn_id as usize].body_hash;
                    self.stack.push(Value::Closure { fn_id, body_hash, captures });
                }
                Op::CallClosure { arity, node_id_idx } => {
                    let arity = arity as usize;
                    // Args sit on the value stack at [args_base..]; the
                    // closure sits just below them at args_base - 1. Take
                    // the closure out (leaving a Unit placeholder), then
                    // write its captures and pop the args directly into
                    // the callee's locals — no per-call args Vec and no
                    // `captures.extend(args)` realloc (#464). The combined
                    // [captures, args] view the tracer wants is exactly
                    // the contiguous locals slice we just filled.
                    let args_base = self.stack.len() - arity;
                    let closure = std::mem::replace(&mut self.stack[args_base - 1], Value::Unit);
                    let (fn_id, captures) = match closure {
                        Value::Closure { fn_id, captures, .. } => (fn_id, captures),
                        other => return Err(VmError::TypeMismatch(format!("CallClosure on non-closure: {other:?}"))),
                    };
                    let fid = fn_id as usize;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    let cap_n = captures.len();
                    let locals_start = self.locals_storage.len();
                    let locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    self.locals_storage.resize(locals_start + locals_len, Value::Unit);
                    for (i, v) in captures.into_iter().enumerate() {
                        self.locals_storage[locals_start + i] = v;
                    }
                    // Move the args off the value stack into the locals
                    // following the captures (popping leaves the args off
                    // the stack; the closure's Unit placeholder is then
                    // the top, so truncate it away).
                    for i in (0..arity).rev() {
                        self.locals_storage[locals_start + cap_n + i] = self.pop()?;
                    }
                    self.stack.truncate(args_base - 1);
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.locals_storage[locals_start..locals_start + cap_n + arity]);
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals_start, locals_len,
                        stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                        // Op::CallClosure intentionally doesn't memoize
                        // for v1 (#229) — closures over captures need a
                        // hashing strategy that includes the captures.
                        // Direct Op::Call is the v1 surface.
                        memo_key: None,
                        stack_record_arena_start: self.stack_record_arena.len(),
                        stack_record_budget_remaining: STACK_RECORD_BUDGET_SLOTS,
                    })?;
                }
                Op::SortByKey { node_id_idx: _ } => {
                    // #338: pop (xs, f). For each x in xs, invoke
                    // f(x) to derive a sortable key. Stable-sort the
                    // (key, value) pairs by key. Return the values
                    // in sorted order. Keys must be Int / Float /
                    // Str; mixed-type pairs and other types compare
                    // as equal (preserving original order — stable
                    // sort).
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("SortByKey requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("SortByKey requires a closure, got: {f:?}")));
                    }
                    let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(items.len());
                    for item in items {
                        let key = self.invoke_closure_1(f.clone(), item.clone())?;
                        keyed.push((key, item));
                    }
                    keyed.sort_by(|(ka, _), (kb, _)| compare_sort_keys(ka, kb));
                    let sorted: VecDeque<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    self.stack.push(Value::List(sorted));
                }
                Op::ParallelMap { node_id_idx: _ } => {
                    // #305 slice 1: pop (xs, f) and apply f to each
                    // element across OS threads.
                    //
                    // #305 slice 2: each worker now asks the parent
                    // handler for a thread-safe per-worker handler via
                    // `EffectHandler::spawn_for_worker`. Handlers that
                    // opt in (e.g. `DefaultHandler`) yield a fresh
                    // instance sharing the budget pool; handlers that
                    // don't fall back to the slice-1 behavior of
                    // `DenyAllEffects` in the worker.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ParallelMap requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ParallelMap requires a closure, got: {f:?}")));
                    }
                    // Pre-build one handler per worker on the main
                    // thread so the worker just owns its handler with
                    // no shared borrowing. The actual worker count is
                    // capped by `LEX_PAR_MAX_CONCURRENCY` (resolved
                    // inside par_map_run); cap ≤ items.len() so we
                    // never over-allocate handlers.
                    let n_workers = par_max_concurrency().max(1).min(items.len().max(1));
                    let mut worker_handlers: Vec<Box<dyn EffectHandler + Send>> =
                        Vec::with_capacity(n_workers);
                    for _ in 0..n_workers {
                        worker_handlers.push(
                            self.handler
                                .spawn_for_worker()
                                .unwrap_or_else(|| Box::new(DenyAllEffects)),
                        );
                    }
                    let results = par_map_run(self.program, f, items.into_iter().collect(), worker_handlers)?;
                    self.stack.push(Value::List(results.into()));
                }
                Op::ListMap { node_id_idx: _ } => {
                    // #464: native map. Owns `xs` (no per-iteration
                    // clone of the input or accumulator that the old
                    // inlined `LoadLocal`-based loop incurred) and
                    // builds the output with one pre-sized allocation.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListMap requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListMap requires a closure, got: {f:?}")));
                    }
                    let mut out: VecDeque<Value> = VecDeque::with_capacity(items.len());
                    for item in items {
                        out.push_back(self.invoke_closure_1(f.clone(), item)?);
                    }
                    self.stack.push(Value::List(out));
                }
                Op::ListFilter { node_id_idx: _ } => {
                    // #464: native filter. Pred is applied to a clone
                    // of each element; the original element is kept on
                    // a true result.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListFilter requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListFilter requires a closure, got: {f:?}")));
                    }
                    let mut out: VecDeque<Value> = VecDeque::new();
                    for item in items {
                        let keep = self.invoke_closure_1(f.clone(), item.clone())?;
                        if keep.as_bool() {
                            out.push_back(item);
                        }
                    }
                    self.stack.push(Value::List(out));
                }
                Op::ListFold { node_id_idx: _ } => {
                    // #464: native left-fold. `acc` is threaded by
                    // value; each element is moved into the combiner.
                    let f = self.pop()?;
                    let init = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListFold requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListFold requires a closure, got: {f:?}")));
                    }
                    let mut acc = init;
                    for item in items {
                        acc = self.invoke_closure_2(f.clone(), acc, item)?;
                    }
                    self.stack.push(acc);
                }
                Op::Call { fn_id, arity, node_id_idx } => {
                    let arity = arity as usize;
                    let fid = fn_id as usize;
                    // Args sit on the value stack at [args_base..]. We
                    // read them in place for the refinement / memo /
                    // trace checks and only move them into the locals
                    // slot-allocator at the very end — avoiding a
                    // per-call args Vec (#464 call-overhead). The stack
                    // naturally holds the args until consumed, so the
                    // only early-exit cleanup is truncating them off on
                    // a memo hit; a refinement error aborts the VM.
                    let args_base = self.stack.len() - arity;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    // Refinement runtime check (#209 slice 3). Each
                    // param's `Option<Refinement>` is evaluated against
                    // the actual arg before the frame is pushed. The
                    // tracer sees the call enter; failure surfaces as
                    // `VmError::RefinementFailed` *before* the body
                    // starts, which means an erroring trace shows the
                    // call as enter+exit_err with the verdict reason
                    // (same shape as `gate.verdict`).
                    //
                    // Iterate by reference — the loop body reads only
                    // through `r` (borrowed from `self.program`) and the
                    // arg slots on the stack; we don't mutate `self`, so
                    // the borrows are disjoint.
                    let refinements = &self.program.functions[fid].refinements;
                    for (i, refinement) in refinements.iter().enumerate() {
                        if let Some(r) = refinement {
                            let arg = self.stack[args_base + i].clone();
                            match eval_refinement(&r.predicate, &r.binding, &arg) {
                                Ok(true) => { /* satisfied, continue */ }
                                Ok(false) => {
                                    return Err(VmError::RefinementFailed {
                                        fn_name: self.program.functions[fid].name.clone(),
                                        param_index: i,
                                        binding: r.binding.clone(),
                                        reason: format!(
                                            "predicate failed for {} = {arg:?}",
                                            r.binding),
                                    });
                                }
                                Err(reason) => {
                                    return Err(VmError::RefinementFailed {
                                        fn_name: self.program.functions[fid].name.clone(),
                                        param_index: i,
                                        binding: r.binding.clone(),
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    // Pure-fn memoization (#229): if the callee declares
                    // no effects, hash the args and consult the cache.
                    // On hit, push the cached value, emit synthetic
                    // enter+exit trace events (so the trace still shows
                    // the call), and skip the frame push entirely.
                    //
                    // Adaptive gate (#229 adaptive): only hash if this
                    // function still has memoization enabled. A pure
                    // function whose args never repeat pays the hash for
                    // nothing; after a warmup window with zero hits we
                    // disable it and its calls take the plain path below.
                    let memo_key: Option<(u32, [u8; 16])> =
                        if self.program.functions[fid].effects.is_empty()
                            && self.memo_fn_state[fid].enabled
                        {
                            Some((fn_id, hash_call_args(&self.stack[args_base..])))
                        } else {
                            if self.program.functions[fid].effects.is_empty() {
                                self.pure_memo_skips += 1;
                            }
                            None
                        };
                    if let Some(key) = memo_key {
                        self.memo_fn_state[fid].calls += 1;
                        if let Some(cached) = self.pure_memo.get(&key).cloned() {
                            self.memo_fn_state[fid].hits += 1;
                            self.pure_memo_hits += 1;
                            self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                            self.tracer.exit_ok(&cached);
                            self.stack.truncate(args_base);
                            self.stack.push(cached);
                            continue;
                        }
                        self.pure_memo_misses += 1;
                        // Disable on a cold function: warmup elapsed with
                        // no hit. Always safe — the callee is pure, so the
                        // plain path recomputes the identical result.
                        let st = &mut self.memo_fn_state[fid];
                        if st.calls >= MEMO_WARMUP_CALLS && st.hits == 0 {
                            st.enabled = false;
                        }
                    }
                    // #465 JIT tier hook. Consulted after refinements +
                    // memo. The hook contract (see `crate::jit_hook`)
                    // requires the dispatcher to emit the synthetic
                    // tracer events itself — we do that on hit, then
                    // truncate the args off the stack and push the
                    // result, mirroring the memo-hit path above.
                    //
                    // Take/restore around the call so the hook can
                    // borrow `&self.stack` for its args slice while
                    // we hold `&mut hook`. Cheaper than cloning the
                    // args; the take/put is two pointer writes.
                    if let Some(mut hook) = self.jit_hook.take() {
                        let hook_result = hook.try_call(fn_id, &self.stack[args_base..]);
                        self.jit_hook = Some(hook);
                        match hook_result? {
                            Some(result) => {
                                self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                                self.tracer.exit_ok(&result);
                                // Memoize the result if memo is enabled
                                // for this fn — same semantics as a
                                // regular call's Return path.
                                if let Some(key) = memo_key {
                                    self.pure_memo.insert(key, result.clone());
                                }
                                self.stack.truncate(args_base);
                                self.stack.push(result);
                                continue;
                            }
                            None => { /* hook declined; fall through */ }
                        }
                    }
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                    let locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    let locals_start = self.locals_storage.len();
                    self.locals_storage.resize(locals_start + locals_len, Value::Unit);
                    // Move the args off the stack into the callee's
                    // locals (popping leaves the stack at `args_base`).
                    for i in (0..arity).rev() {
                        self.locals_storage[locals_start + i] = self.pop()?;
                    }
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals_start, locals_len,
                        stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                        memo_key,
                        stack_record_arena_start: self.stack_record_arena.len(),
                        stack_record_budget_remaining: STACK_RECORD_BUDGET_SLOTS,
                    })?;
                }
                Op::TailCall { fn_id, arity, node_id_idx } => {
                    let arity = arity as usize;
                    let fid = fn_id as usize;
                    // Args sit on the value stack at [args_base..]. Read
                    // them in place for the refinement / trace checks and
                    // move them into the reused frame's locals at the end
                    // — no per-call args Vec (#464). Tail calls have no
                    // memoization, so the consumers are refinement, trace,
                    // then the locals move. The args live on `self.stack`
                    // while locals live on `self.locals_storage`, so the
                    // `truncate(old_locals_start)` below (which releases
                    // the *old* frame's locals) doesn't touch them.
                    let args_base = self.stack.len() - arity;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    // Refinement runtime check on tail calls too
                    // (#209 slice 3). Same shape as Op::Call.
                    let refinements = &self.program.functions[fid].refinements;
                    for (i, refinement) in refinements.iter().enumerate() {
                        if let Some(r) = refinement {
                            let arg = self.stack[args_base + i].clone();
                            match eval_refinement(&r.predicate, &r.binding, &arg) {
                                Ok(true) => {}
                                Ok(false) => return Err(VmError::RefinementFailed {
                                    fn_name: self.program.functions[fid].name.clone(),
                                    param_index: i,
                                    binding: r.binding.clone(),
                                    reason: format!(
                                        "predicate failed for {} = {arg:?}",
                                        r.binding),
                                }),
                                Err(reason) => return Err(VmError::RefinementFailed {
                                    fn_name: self.program.functions[fid].name.clone(),
                                    param_index: i,
                                    binding: r.binding.clone(),
                                    reason,
                                }),
                            }
                        }
                    }
                    // A tail call closes the current call's trace frame and
                    // opens a new one in its place — preserves the caller's
                    // tree depth in the trace.
                    self.tracer.exit_call_tail();
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                    // Reuse the current frame's locals_start position:
                    // truncate to release old locals then extend for the
                    // new function (#389 slice 3, same as Op::Return but
                    // without popping the frame).
                    let old_locals_start = self.frames.last().unwrap().locals_start;
                    self.locals_storage.truncate(old_locals_start);
                    let new_locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    self.locals_storage.resize(old_locals_start + new_locals_len, Value::Unit);
                    // Move the args off the value stack into the callee's
                    // locals (popping leaves the stack at `args_base`).
                    for i in (0..arity).rev() {
                        self.locals_storage[old_locals_start + i] = self.pop()?;
                    }
                    // #464 step 2: a tail-called function gets a fresh
                    // stack-record arena view. Release any records the
                    // pre-tail-call code allocated (they can't be live
                    // — the args have already been popped off the
                    // value stack) and refill the budget for the
                    // callee.
                    let arena_start = self.frames.last().unwrap().stack_record_arena_start;
                    self.stack_record_arena.truncate(arena_start);
                    let frame = self.frames.last_mut().unwrap();
                    frame.fn_id = fn_id;
                    frame.pc = 0;
                    frame.locals_len = new_locals_len;
                    frame.trace_kind = FrameKind::Call(node_id);
                    frame.stack_record_budget_remaining = STACK_RECORD_BUDGET_SLOTS;
                }
                Op::EffectCall { kind_idx, op_idx, arity, node_id_idx } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let kind = match &self.program.constants[kind_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected Str const for effect kind".into())),
                    };
                    let op_name = match &self.program.constants[op_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected Str const for effect op".into())),
                    };
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    self.tracer.enter_effect(&node_id, &kind, &op_name, &args);
                    let result = match self.tracer.override_effect(&node_id) {
                        Some(v) => Ok(v),
                        // VM-level intercept for `parser.run` (#221).
                        // Routed inline rather than through the handler
                        // because the parser interpreter needs reentrant
                        // VM access to invoke `Value::Closure` values
                        // from `Map` / `AndThen` nodes.
                        None if (kind.as_str(), op_name.as_str()) == ("parser", "run")
                            => self.run_parser_op(args),
                        // VM-level intercept for `conc.*` (#381). The actor
                        // handler closure must run on the calling VM so it can
                        // dispatch arbitrary effects through the same handler
                        // chain (e.g. sql queries inside an actor).
                        None if kind.as_str() == "conc"
                            => self.run_conc_op(op_name.as_str(), args),
                        None => self.handler.dispatch(&kind, &op_name, args),
                    };
                    match result {
                        Ok(v) => {
                            self.tracer.exit_ok(&v);
                            self.stack.push(v);
                        }
                        Err(e) => {
                            self.tracer.exit_err(&e);
                            return Err(VmError::Effect(e));
                        }
                    }
                }
                Op::Return => {
                    let v = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    // Trim any extra stuff that the function pushed but didn't pop.
                    self.stack.truncate(frame.stack_base);
                    // Release this frame's locals back to the arena (#389 slice 3).
                    // LIFO frame ordering guarantees this frame's slots are at the top.
                    self.locals_storage.truncate(frame.locals_start);
                    // #464 step 2: release this frame's stack-record
                    // slab. LIFO frame discipline guarantees its
                    // records sit at the top of the arena. The
                    // returned value `v` is escape-proven not to be
                    // one of them — the compiler only emits
                    // AllocStackRecord at sites that don't reach
                    // `Return`.
                    self.stack_record_arena.truncate(frame.stack_record_arena_start);
                    if matches!(frame.trace_kind, FrameKind::Call(_)) {
                        self.tracer.exit_ok(&v);
                    }
                    // Pure-fn memoization (#229): if this frame was a
                    // memoizable call that missed the cache, write the
                    // computed return value back so the next call with
                    // the same args returns it without re-executing.
                    if let Some(key) = frame.memo_key {
                        self.pure_memo.insert(key, v.clone());
                    }
                    // Exit when we've returned past the depth this
                    // `run_to` was entered at — supports reentrancy
                    // (a nested `invoke` returns into its caller, not
                    // out of the outermost VM run, #221).
                    if self.frames.len() <= base_depth {
                        return Ok(v);
                    }
                    self.stack.push(v);
                }
                Op::Panic(i) => {
                    let msg = match &self.program.constants[i as usize] {
                        Const::Str(s) => s.clone(),
                        _ => "panic".into(),
                    };
                    return Err(VmError::Panic(msg));
                }
                // Arithmetic
                Op::IntAdd => self.bin_int(|a, b| Value::Int(a + b))?,
                Op::IntSub => self.bin_int(|a, b| Value::Int(a - b))?,
                Op::IntMul => self.bin_int(|a, b| Value::Int(a * b))?,
                Op::IntDiv => self.bin_int(|a, b| Value::Int(a / b))?,
                Op::IntMod => self.bin_int(|a, b| Value::Int(a % b))?,
                Op::IntNeg => {
                    let a = self.pop()?.as_int();
                    self.stack.push(Value::Int(-a));
                }
                Op::IntEq => self.bin_int(|a, b| Value::Bool(a == b))?,
                Op::IntLt => self.bin_int(|a, b| Value::Bool(a < b))?,
                Op::IntLe => self.bin_int(|a, b| Value::Bool(a <= b))?,
                Op::FloatAdd => self.bin_float(|a, b| Value::Float(a + b))?,
                Op::FloatSub => self.bin_float(|a, b| Value::Float(a - b))?,
                Op::FloatMul => self.bin_float(|a, b| Value::Float(a * b))?,
                Op::FloatDiv => self.bin_float(|a, b| Value::Float(a / b))?,
                Op::FloatNeg => {
                    let a = self.pop()?.as_float();
                    self.stack.push(Value::Float(-a));
                }
                Op::FloatEq => self.bin_float(|a, b| Value::Bool(a == b))?,
                Op::FloatLt => self.bin_float(|a, b| Value::Bool(a < b))?,
                Op::FloatLe => self.bin_float(|a, b| Value::Bool(a <= b))?,
                Op::NumAdd => {
                    // #308: `+` is overloaded — Str+Str concatenates,
                    // numerics add. Other arithmetic ops (-, *, /, %)
                    // still reject Str at the type-checker layer.
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => self.stack.push(Value::Int(x + y)),
                        (Value::Float(x), Value::Float(y)) => self.stack.push(Value::Float(x + y)),
                        (Value::Str(x), Value::Str(y)) => {
                            // SmolStr is immutable; concatenate via a temporary String.
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(&x);
                            s.push_str(&y);
                            self.stack.push(Value::Str(s.into()));
                        }
                        (a, b) => return Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
                    }
                }
                Op::NumSub => self.bin_num(|a, b| Value::Int(a - b), |a, b| Value::Float(a - b))?,
                Op::NumMul => self.bin_num(|a, b| Value::Int(a * b), |a, b| Value::Float(a * b))?,
                Op::NumDiv => self.bin_num(|a, b| Value::Int(a / b), |a, b| Value::Float(a / b))?,
                Op::NumMod => self.bin_int(|a, b| Value::Int(a % b))?,
                Op::NumNeg => {
                    let v = self.pop()?;
                    match v {
                        Value::Int(n) => self.stack.push(Value::Int(-n)),
                        Value::Float(f) => self.stack.push(Value::Float(-f)),
                        other => return Err(VmError::TypeMismatch(format!("NumNeg on {other:?}"))),
                    }
                }
                Op::NumEq => self.bin_eq()?,
                Op::NumLt => self.bin_ord(|a, b| Value::Bool(a < b), |a, b| Value::Bool(a < b), |a, b| Value::Bool(a < b))?,
                Op::NumLe => self.bin_ord(|a, b| Value::Bool(a <= b), |a, b| Value::Bool(a <= b), |a, b| Value::Bool(a <= b))?,
                Op::BoolAnd => {
                    let b = self.pop()?.as_bool();
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(a && b));
                }
                Op::BoolOr => {
                    let b = self.pop()?.as_bool();
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(a || b));
                }
                Op::BoolNot => {
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(!a));
                }
                Op::StrConcat => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let s = format!("{}{}", a.as_str(), b.as_str());
                    self.stack.push(Value::Str(s.into()));
                }
                Op::StrLen => {
                    let v = self.pop()?;
                    self.stack.push(Value::Int(v.as_str().len() as i64));
                }
                Op::StrEq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(a.as_str() == b.as_str()));
                }
                Op::BytesLen => {
                    let v = self.pop()?;
                    match v {
                        Value::Bytes(b) => self.stack.push(Value::Int(b.len() as i64)),
                        other => return Err(VmError::TypeMismatch(format!("BytesLen on {other:?}"))),
                    }
                }
                Op::BytesEq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let eq = match (a, b) {
                        (Value::Bytes(x), Value::Bytes(y)) => x == y,
                        _ => return Err(VmError::TypeMismatch("BytesEq operands".into())),
                    };
                    self.stack.push(Value::Bool(eq));
                }

                // Superinstructions (#461).
                Op::LoadLocalAddIntConst { local_idx, imm_const_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + local_idx as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        c => return Err(VmError::TypeMismatch(
                            format!("LoadLocalAddIntConst expected Int const, got {c:?}"))),
                    };
                    self.stack.push(Value::Int(a + b));
                    // Override the default `pc + 1`: skip past the
                    // two inert primitive ops (the original
                    // PushConst + IntAdd) that the peephole pass
                    // left in place for body-hash stability.
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalAddLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a + b));
                    // Override the default `pc + 1`: skip past the
                    // two inert primitive ops (the original
                    // LoadLocal(rhs_idx) + IntAdd) that the peephole
                    // pass left in place for body-hash stability.
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalSubLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a - b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalMulLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a * b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetField { local_idx, name_idx, site_idx } => {
                    // #461 slice 9: fused `LoadLocal + GetField`. Reads
                    // the field directly out of the local record by
                    // reference and pushes it, advancing pc by 2 (one
                    // tombstone — the original GetField). Avoids the
                    // unfused pair's whole-record clone onto the value
                    // stack: the dominant heap-record churn on the
                    // `response_build` profile (`r.total` field reads).
                    let base = self.frames[frame_idx].locals_start;
                    let v = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetField")?;
                    self.stack.push(v);
                    self.frames[frame_idx].pc = pc + 2;
                }
                Op::LoadLocalGetFieldAdd { local_idx, name_idx, site_idx } => {
                    // #461 slice 7: fused `LoadLocal + GetField + IntAdd`.
                    // Pop the prior stack top (the accumulator), read the
                    // field by reference (shared IC via
                    // `read_local_record_field`), push the sum, advance
                    // pc by 3 (skip the GetField and IntAdd tombstones).
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldAdd")?.as_int();
                    self.stack.push(Value::Int(acc + b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetFieldSub { local_idx, name_idx, site_idx } => {
                    // #461 slice 8: `LoadLocal + GetField + IntSub`. The
                    // `acc - r.field` idiom. IntSub computes
                    // deeper-minus-top; the field was on top in the
                    // unfused form, so the result is `acc - field`.
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldSub")?.as_int();
                    self.stack.push(Value::Int(acc - b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetFieldMul { local_idx, name_idx, site_idx } => {
                    // #461 slice 8: `LoadLocal + GetField + IntMul`. The
                    // `acc * r.field` idiom (mul is commutative, so
                    // operand order doesn't matter).
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldMul")?.as_int();
                    self.stack.push(Value::Int(acc * b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalEqIntConstJumpIfNot { local_idx, imm_const_idx, jump_offset } => {
                    // First jump-aware fusion (#461 slice 5). The
                    // JumpIfNot's offset is relative to its own
                    // pc + 1 = (pc + 3) + 1 = pc + 4, so the branch
                    // target is `pc + 4 + jump_offset`. Fall-through
                    // (equal → JumpIfNot doesn't jump) is `pc + 4`
                    // (skip past the 3 tombstones — PushConst +
                    // IntEq + JumpIfNot).
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + local_idx as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        _ => return Err(VmError::TypeMismatch(
                            "LoadLocalEqIntConstJumpIfNot expects Const::Int".into())),
                    };
                    let next_pc = if a == b {
                        pc + 4
                    } else {
                        ((pc as i32 + 4) + jump_offset) as usize
                    };
                    self.frames[frame_idx].pc = next_pc;
                }
                Op::LoadLocalStoreEqIntConstJumpIfNot { src, dst, imm_const_idx, jump_offset } => {
                    // Slice 6: absorbs LoadLocal + StoreLocal + slice-5 op.
                    // 6-slot window total (this op + 5 tombstones); fall-
                    // through is `pc + 6`, branch target is `pc + 6 +
                    // jump_offset` (the original JumpIfNot was at slot
                    // pc+5, with offset relative to its own pc+1 = pc+6).
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + src as usize].as_int();
                    // Mirror the original `StoreLocal(dst)` — later
                    // arm tests in the same `match` expect to find
                    // the scrutinee at `locals[dst]`.
                    self.locals_storage[base + dst as usize] = Value::Int(a);
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        _ => return Err(VmError::TypeMismatch(
                            "LoadLocalStoreEqIntConstJumpIfNot expects Const::Int".into())),
                    };
                    let next_pc = if a == b {
                        pc + 6
                    } else {
                        ((pc as i32 + 6) + jump_offset) as usize
                    };
                    self.frames[frame_idx].pc = next_pc;
                }
                Op::LoadLocalAddIntConstStoreLocal { src, imm_const_idx, dest } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + src as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        c => return Err(VmError::TypeMismatch(
                            format!("LoadLocalAddIntConstStoreLocal expected Int const, got {c:?}"))),
                    };
                    self.locals_storage[base + dest as usize] = Value::Int(a + b);
                    // Skip past the 3 inert primitive ops we
                    // absorbed (original PushConst + IntAdd +
                    // StoreLocal).
                    self.frames[frame_idx].pc = pc + 4;
                }
            }
        }
    }

    fn pop(&mut self) -> Result<Value, VmError> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }
    fn peek(&self) -> Result<&Value, VmError> {
        self.stack.last().ok_or(VmError::StackUnderflow)
    }

    /// IC-cached field read of `locals[local_idx]`, shared by the
    /// field-read fusions: slice 9's `LoadLocalGetField` and slice
    /// 7/8's `LoadLocalGetField{Add,Sub,Mul}`. Uses the same
    /// `(fn_id, site_idx)` inline-cache slot as the unfused
    /// `Op::GetField`, so the paths stay cache-consistent.
    /// `op_name` only appears in the non-record error message.
    ///
    /// Reads the record **by reference** and clones out only the
    /// selected field — it does *not* clone the whole record. The
    /// unfused `[LoadLocal, GetField]` pair clones the entire record
    /// (`Box<IndexMap>` for a heap record) onto the value stack just
    /// to read one field and drop the rest; on the `response_build`
    /// profile that whole-record clone+drop of the returned `Response`
    /// dominated the malloc traffic. Borrowing in place removes it.
    ///
    /// Borrow discipline: the inline-cache slot can't be written while
    /// the record (a borrow of `self.locals_storage`) is live, so the
    /// match yields `(value, install)` and the `field_ics` write
    /// happens after the borrow ends.
    ///
    /// `#[inline(always)]`: hot dispatch path, called from four tight
    /// `run_to` arms; leaving it out-of-line showed up as a standalone
    /// call frame on the profile.
    #[inline(always)]
    fn read_local_record_field(
        &mut self,
        base: usize,
        local_idx: u16,
        fn_id: u32,
        name_idx: u32,
        site_idx: u32,
        op_name: &str,
    ) -> Result<Value, VmError> {
        let fid = fn_id as usize;
        let sid = site_idx as usize;
        if self.field_ics[fid].is_empty() {
            let n = self.program.functions[fid].field_ic_sites as usize;
            self.field_ics[fid] = vec![None; n];
        }
        let cached = self.field_ics[fid][sid];
        let li = base + local_idx as usize;

        let (value, install): (Value, Option<(u32, usize)>) =
            match &self.locals_storage[li] {
                Value::Record { fields: r, shape_id } => {
                    let shape_id = *shape_id;
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    let hit = if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id {
                            if shape_id != crate::value::NO_SHAPE_ID {
                                r.get_index(off).map(|(_, val)| val.clone())
                            } else if let Some((k, val)) = r.get_index(off) {
                                match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) if s == k => Some(val.clone()),
                                    _ => None,
                                }
                            } else { None }
                        } else { None }
                    } else { None };
                    match hit {
                        Some(v) => (v, None),
                        None => {
                            let name = match &self.program.constants[name_idx as usize] {
                                Const::FieldName(s) => s.as_str(),
                                _ => return Err(VmError::TypeMismatch(
                                    "expected FieldName const".into())),
                            };
                            let (off, _, val) = r.get_full(name)
                                .ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{name}`")))?;
                            (val.clone(), Some((shape_id, off)))
                        }
                    }
                }
                &Value::StackRecord { shape_id, slab_start, field_count } => {
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id && (off as u16) < field_count {
                            let idx = slab_start as usize + off;
                            (self.stack_record_arena[idx].clone(), None)
                        } else {
                            let off = self.resolve_stack_field(shape_id, name_idx)?;
                            (self.stack_record_arena[slab_start as usize + off].clone(),
                             Some((shape_id, off)))
                        }
                    } else {
                        let off = self.resolve_stack_field(shape_id, name_idx)?;
                        (self.stack_record_arena[slab_start as usize + off].clone(),
                         Some((shape_id, off)))
                    }
                }
                // #463 slice 2a: superinstruction read out of an
                // arena-allocated record held in a local. Same shape
                // resolution as the stack-record arm (records share
                // the same `record_shapes` table regardless of
                // allocation site); only the slab indexed differs.
                &Value::ArenaRecord { shape_id, slab_start, field_count } => {
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id && (off as u16) < field_count {
                            let idx = slab_start as usize + off;
                            (self.arena_slab[idx].clone(), None)
                        } else {
                            let off = self.resolve_stack_field(shape_id, name_idx)?;
                            (self.arena_slab[slab_start as usize + off].clone(),
                             Some((shape_id, off)))
                        }
                    } else {
                        let off = self.resolve_stack_field(shape_id, name_idx)?;
                        (self.arena_slab[slab_start as usize + off].clone(),
                         Some((shape_id, off)))
                    }
                }
                other => return Err(VmError::TypeMismatch(
                    format!("{op_name} on non-record: {other:?}"))),
            };
        if let Some(entry) = install {
            self.field_ics[fid][sid] = Some(entry);
        }
        Ok(value)
    }

    /// Resolve a field offset within a stack-record shape by name
    /// (the slow path when the inline cache misses). Factored out so
    /// `read_local_record_field` doesn't hold the `locals_storage`
    /// borrow across the `record_shapes` / `constants` walk.
    #[inline]
    fn resolve_stack_field(&self, shape_id: u32, name_idx: u32) -> Result<usize, VmError> {
        let shape = &self.program.record_shapes[shape_id as usize];
        let target_name = match &self.program.constants[name_idx as usize] {
            Const::FieldName(s) => s.as_str(),
            _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
        };
        for (i, fn_const_idx) in shape.iter().enumerate() {
            if let Const::FieldName(s) = &self.program.constants[*fn_const_idx as usize] {
                if s == target_name { return Ok(i); }
            }
        }
        Err(VmError::TypeMismatch(
            format!("missing field `{target_name}` on stack record")))
    }

    fn bin_int(&mut self, f: impl Fn(i64, i64) -> Value) -> Result<(), VmError> {
        let b = self.pop()?.as_int();
        let a = self.pop()?.as_int();
        self.stack.push(f(a, b));
        Ok(())
    }
    fn bin_float(&mut self, f: impl Fn(f64, f64) -> Value) -> Result<(), VmError> {
        let b = self.pop()?.as_float();
        let a = self.pop()?.as_float();
        self.stack.push(f(a, b));
        Ok(())
    }
    fn bin_num(
        &mut self,
        i: impl Fn(i64, i64) -> Value,
        f: impl Fn(f64, f64) -> Value,
    ) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => { self.stack.push(i(x, y)); Ok(()) }
            (Value::Float(x), Value::Float(y)) => { self.stack.push(f(x, y)); Ok(()) }
            (a, b) => Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
        }
    }

    /// Like `bin_num` but also handles `Str` operands via lexicographic order.
    /// Used by `NumLt` / `NumLe` because the type checker admits `Str < Str`
    /// and `>` / `>=` compile as swap+NumLt / swap+NumLe (#332).
    fn bin_ord(
        &mut self,
        i: impl Fn(i64, i64) -> Value,
        f: impl Fn(f64, f64) -> Value,
        s: impl Fn(&str, &str) -> Value,
    ) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => { self.stack.push(i(x, y)); Ok(()) }
            (Value::Float(x), Value::Float(y)) => { self.stack.push(f(x, y)); Ok(()) }
            (Value::Str(x), Value::Str(y)) => { self.stack.push(s(&x, &y)); Ok(()) }
            (a, b) => Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
        }
    }
    fn bin_eq(&mut self) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.stack.push(Value::Bool(a == b));
        Ok(())
    }
}

impl Drop for Vm<'_> {
    fn drop(&mut self) {
        if ic_stats_enabled() {
            dump_ic_stats();
        }
    }
}

/// Construct a `Value::Variant` with the given name and args.
/// Used by `conc.*` registry ops to return `Result`/`Option`/`ConcError`
/// values without hand-writing the struct literal at every site.
fn variant(name: &str, args: Vec<Value>) -> Value {
    Value::Variant { name: name.to_string(), args }
}
fn variant_ok(payload: Value) -> Value { variant("Ok", vec![payload]) }
fn variant_err(payload: Value) -> Value { variant("Err", vec![payload]) }

fn const_to_value(c: &Const) -> Value {
    match c {
        Const::Int(n) => Value::Int(*n),
        Const::Float(f) => Value::Float(*f),
        Const::Bool(b) => Value::Bool(*b),
        Const::Str(s) => Value::Str(s.as_str().into()),
        Const::Bytes(b) => Value::Bytes(b.clone()),
        Const::Unit => Value::Unit,
        Const::FieldName(s) | Const::VariantName(s) | Const::NodeId(s) => Value::Str(s.as_str().into()),
    }
}

#[cfg(test)]
mod memo_hash_tests {
    //! #461 follow-up: invariants for the structural memo-key hash
    //! that replaced the SHA-256-over-canonical-JSON path. The memo
    //! cache keys on this digest with no equality fallback, so the
    //! load-bearing property is "equal-under-PartialEq args produce
    //! an equal key" — plus enough discrimination that distinct args
    //! don't collide in practice.
    use super::*;
    use indexmap::IndexMap;

    fn rec(pairs: &[(&str, Value)]) -> Value {
        let mut m: IndexMap<SmolStr, Value> = IndexMap::new();
        for (k, v) in pairs { m.insert((*k).into(), v.clone()); }
        Value::Record { shape_id: crate::value::NO_SHAPE_ID, fields: Box::new(m) }
    }

    #[test]
    fn identical_args_hash_equal() {
        let a = vec![Value::Int(7), Value::Str("hi".into())];
        let b = vec![Value::Int(7), Value::Str("hi".into())];
        assert_eq!(hash_call_args(&a), hash_call_args(&b));
    }

    #[test]
    fn distinct_scalars_differ() {
        assert_ne!(hash_call_args(&[Value::Int(7)]), hash_call_args(&[Value::Int(8)]));
        assert_ne!(hash_call_args(&[Value::Int(0)]), hash_call_args(&[Value::Bool(false)]));
        assert_ne!(hash_call_args(&[Value::Int(0)]), hash_call_args(&[Value::Unit]));
        assert_ne!(hash_call_args(&[Value::Bool(true)]), hash_call_args(&[Value::Bool(false)]));
    }

    #[test]
    fn arity_is_part_of_the_key() {
        assert_ne!(
            hash_call_args(&[Value::Int(1), Value::Int(2)]),
            hash_call_args(&[Value::Int(1)]),
        );
        // A 2-arg call vs a single Tuple arg of the same elements
        // must not collide.
        assert_ne!(
            hash_call_args(&[Value::Int(1), Value::Int(2)]),
            hash_call_args(&[Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
        );
    }

    #[test]
    fn record_hash_is_field_order_independent() {
        // IndexMap equality ignores insertion order, so the key must
        // too — otherwise equal records would miss the cache.
        let r1 = rec(&[("a", Value::Int(1)), ("b", Value::Int(2))]);
        let r2 = rec(&[("b", Value::Int(2)), ("a", Value::Int(1))]);
        assert_eq!(r1, r2, "precondition: records compare equal");
        assert_eq!(hash_call_args(&[r1]), hash_call_args(&[r2]));
    }

    #[test]
    fn record_distinguishes_values_and_keys() {
        let base = rec(&[("a", Value::Int(1)), ("b", Value::Int(2))]);
        let diff_val = rec(&[("a", Value::Int(1)), ("b", Value::Int(3))]);
        let diff_key = rec(&[("a", Value::Int(1)), ("c", Value::Int(2))]);
        assert_ne!(hash_call_args(std::slice::from_ref(&base)), hash_call_args(&[diff_val]));
        assert_ne!(hash_call_args(&[base]), hash_call_args(&[diff_key]));
    }

    #[test]
    fn shape_id_does_not_affect_record_key() {
        // PartialEq ignores shape_id; the key must too.
        let mut m: IndexMap<SmolStr, Value> = IndexMap::new();
        m.insert("a".into(), Value::Int(1));
        let r_no_shape = Value::Record { shape_id: crate::value::NO_SHAPE_ID, fields: Box::new(m.clone()) };
        let r_shaped = Value::Record { shape_id: 3, fields: Box::new(m) };
        assert_eq!(r_no_shape, r_shaped);
        assert_eq!(hash_call_args(&[r_no_shape]), hash_call_args(&[r_shaped]));
    }

    #[test]
    fn variant_name_and_args_matter() {
        let some1 = Value::Variant { name: "Some".into(), args: vec![Value::Int(1)] };
        let some1b = Value::Variant { name: "Some".into(), args: vec![Value::Int(1)] };
        let some2 = Value::Variant { name: "Some".into(), args: vec![Value::Int(2)] };
        let none = Value::Variant { name: "None".into(), args: vec![] };
        assert_eq!(hash_call_args(std::slice::from_ref(&some1)), hash_call_args(&[some1b]));
        assert_ne!(hash_call_args(std::slice::from_ref(&some1)), hash_call_args(&[some2]));
        assert_ne!(hash_call_args(&[some1]), hash_call_args(&[none]));
    }

    #[test]
    fn float_bit_pattern_keys() {
        assert_eq!(hash_call_args(&[Value::Float(1.5)]), hash_call_args(&[Value::Float(1.5)]));
        assert_ne!(hash_call_args(&[Value::Float(1.5)]), hash_call_args(&[Value::Float(2.5)]));
        // Same NaN bit pattern → same key (harmless: pure callee is
        // deterministic on bit-identical args).
        let nan = f64::NAN;
        assert_eq!(hash_call_args(&[Value::Float(nan)]), hash_call_args(&[Value::Float(nan)]));
    }
}
