//! M5: bytecode VM. Stack machine with effect dispatch through a host handler.

use crate::op::*;
use crate::program::*;
use crate::value::{ActorCell, Value};
use std::sync::{Arc, Mutex, OnceLock};
use indexmap::IndexMap;
use smol_str::SmolStr;
use std::collections::{HashMap, VecDeque};

mod closures;
mod dispatch;
mod memo;
mod native_list;

use memo::*;
use native_list::*;

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
    /// Integer division or modulo with a zero divisor (#696). Without
    /// this guard the host `/`/`%` panics and takes the whole process
    /// down — the crash report had a conformance harness compute a
    /// rate over an empty set in teardown, far from any user-visible
    /// division. Surfacing a catchable `VmError` instead keeps the
    /// failure inside the language's error model. Float div/mod is
    /// exempt: IEEE-754 yields inf/NaN rather than trapping.
    #[error("integer {op} by zero")]
    DivByZero {
        /// `"division"` or `"modulo"` — names the offending operator.
        op: &'static str,
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
                        // #698: when `ask`/`tell` runs inside a `net.serve` worker, an
                        // arena request-scope is active, so the handler's `(new_state,
                        // reply)` tuple is allocated as a `Value::ArenaTuple` rather than
                        // a heap `Value::Tuple` — and the bare match below would reject it.
                        // Materialize arena handles into heap-owned form NOW, while the
                        // producing scope is still active: the reply crosses back to the
                        // caller and `new_state` persists in the actor cell beyond this
                        // request's arena scope, so both must be heap-owned. Idempotent
                        // (a no-op walk) when there are no arena handles, e.g. from `main`.
                        let result = self.materialize_arena_handles(result);
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
        // unconditionally for refinement-clean calls. Pass the step
        // counter + limit so JITed loops can account against the
        // VM's DoS guard (architectural fix; see jit_hook.rs).
        if let Some(mut hook) = self.jit_hook.take() {
            let step_ptr = &mut self.steps as *mut u64;
            let limit = self.step_limit;
            let hook_result = hook.try_call(fn_id, &args, step_ptr, limit);
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
