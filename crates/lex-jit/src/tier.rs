//! Tier-up integration: a [`JitVm`] wrapper that intercepts
//! [`Vm::call`] and routes eligible workloads through the JIT.
//!
//! ## What it does
//!
//! [`JitVm`] wraps an existing `Vm` plus an owned [`JitContext`]
//! and a per-function `JitFnState` cache (keyed by `fn_id` within
//! the wrapped `Program`). When `JitVm::call(name, args)` is
//! invoked:
//!
//! 1. Look up `fn_id` from the program. If unknown, surface
//!    `VmError::UnknownFunction` (same shape as `Vm::call`).
//! 2. Bump the per-fn call counter. If the function's
//!    `JitFnState::compiled` is still `None` and the counter has
//!    reached `threshold`, attempt compilation:
//!    - If `is_jit_eligible` returns false → mark as `Ineligible`,
//!      fall through to the interpreter from now on.
//!    - If compilation fails for any reason → also mark as
//!      `Ineligible` (defensive; the eligibility predicate should
//!      have caught it, but Cranelift codegen has its own failure
//!      modes).
//!    - On success → cache the [`JittedFn`].
//! 3. If the function is JITed *and* all args unbox cleanly to
//!    `i64` (`Value::Int(_)` or `Value::Bool(_)`), invoke the
//!    native code and wrap the result back into a `Value`.
//! 4. Otherwise — eligible but args don't unbox, or never JITed —
//!    forward to `Vm::call`.
//!
//! ## What it does NOT do
//!
//! - **`Op::Call` interception.** Internal calls between Lex
//!   functions still flow through the interpreter even when the
//!   callee has been JITed. The wrapper only fires on the
//!   outermost `JitVm::call` entry. A real tier-up integration
//!   needs to hook the dispatch loop's `Op::Call` arm, which
//!   would require either restructuring the crate graph
//!   (lex-bytecode would need to depend on lex-jit, which depends
//!   on lex-bytecode — currently circular) or threading a runtime
//!   callback through. Deferred.
//! - **Tier-down / deopt.** Once a function is `Ineligible` it
//!   stays that way for the life of the `JitVm`. Programs that
//!   would benefit from re-evaluation (e.g. an eligible function
//!   first called with a non-int arg, then later called with int
//!   args) are not retroactively JITed — they just stay on the
//!   interpreter path.
//! - **Effect handler hand-off.** The wrapper uses whatever
//!   handler the underlying `Vm` was constructed with for the
//!   interp path. JITed code can't invoke effects (the op set is
//!   the MVP arith subset), so this is a non-issue today.

use lex_bytecode::program::Program;
use lex_bytecode::value::Value;
use lex_bytecode::vm::{Vm, VmError};

use crate::{is_jit_eligible, JitContext, JitError, JittedFn};

/// JIT cache state for a single function in the wrapped program.
///
/// `counter` ticks each time `JitVm::call` lands on this fn. On
/// reaching `threshold` we evaluate eligibility — that gate is
/// only consulted *once* per function, after which `compiled`
/// transitions to a terminal state (either a [`JittedFn`] or
/// `Ineligible`).
enum CompileState {
    /// Not yet evaluated. Holds the call counter so we can
    /// implement a tier-up threshold without recomputing.
    Pending { counter: u32 },
    /// Tried and rejected — either `is_jit_eligible` said no, or
    /// codegen errored. Routes through the interpreter forever.
    Ineligible,
    /// Compiled. The pointer lives in the parent `JitContext`'s
    /// `JITModule`; safe to call as long as the `JitVm` is alive.
    Compiled(JittedFn),
}

/// JIT tier wrapping a [`Vm`]. Owns a [`JitContext`] and a
/// per-function cache; intercepts [`JitVm::call`] to route
/// eligible workloads through native code while forwarding
/// everything else to the interpreter unchanged.
pub struct JitVm<'a> {
    vm: Vm<'a>,
    ctx: JitContext,
    /// Cache indexed by `fn_id`. Length matches
    /// `program.functions.len()` at construction.
    cache: Vec<CompileState>,
    /// Number of `JitVm::call` invocations a function must accrue
    /// before we attempt compilation. Default `1` — eager — so
    /// the first eligible call already runs native.
    threshold: u32,
    program: &'a Program,
}

impl<'a> JitVm<'a> {
    /// Build a tier over `program` using the standard
    /// `Vm::new(program)` interpreter (no custom effect handler).
    /// Threshold defaults to `1` (eager). Use
    /// [`JitVm::with_threshold`] to defer compilation.
    pub fn new(program: &'a Program) -> Result<Self, JitError> {
        Self::with_threshold(program, 1)
    }

    pub fn with_threshold(program: &'a Program, threshold: u32) -> Result<Self, JitError> {
        let cache = (0..program.functions.len())
            .map(|_| CompileState::Pending { counter: 0 })
            .collect();
        Ok(Self {
            vm: Vm::new(program),
            ctx: JitContext::new()?,
            cache,
            threshold: threshold.max(1),
            program,
        })
    }

    /// Borrow the underlying interpreter — exposed so callers can
    /// drive `Vm::set_step_limit` and similar one-shot config.
    pub fn vm_mut(&mut self) -> &mut Vm<'a> {
        &mut self.vm
    }

    /// Same shape as [`Vm::call`]. Routes through the JIT when
    /// possible (see module docs), otherwise to the interpreter.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let fn_id = self
            .program
            .lookup(name)
            .ok_or_else(|| VmError::UnknownFunction(name.to_string()))?
            as usize;

        // Step 1 — bump counter, maybe transition Pending → Compiled / Ineligible.
        if let CompileState::Pending { counter } = &mut self.cache[fn_id] {
            *counter += 1;
            if *counter >= self.threshold {
                self.cache[fn_id] = self.attempt_compile(fn_id);
            }
        }

        // Step 2 — dispatch.
        match &self.cache[fn_id] {
            CompileState::Compiled(jitted) => {
                if let Some(unboxed) = unbox_args(&args, jitted.arity()) {
                    let r = unsafe { jitted.call(&unboxed) };
                    return Ok(Value::Int(r));
                }
                // Eligible but the arg shape doesn't fit (e.g. a
                // record was passed where Int was expected, which
                // shouldn't normally happen for an eligible
                // function — but be defensive).
                self.vm.call(name, args)
            }
            CompileState::Ineligible | CompileState::Pending { .. } => self.vm.call(name, args),
        }
    }

    fn attempt_compile(&mut self, fn_id: usize) -> CompileState {
        let f = &self.program.functions[fn_id];
        if !is_jit_eligible(f, &self.program.constants) {
            return CompileState::Ineligible;
        }
        match self.ctx.compile(f, &self.program.constants) {
            Ok(jitted) => CompileState::Compiled(jitted),
            Err(_) => CompileState::Ineligible,
        }
    }

    /// Diagnostic: how many functions in the cache are in each
    /// state right now. Useful for tests and benches.
    pub fn cache_stats(&self) -> CacheStats {
        let mut s = CacheStats::default();
        for st in &self.cache {
            match st {
                CompileState::Pending { .. } => s.pending += 1,
                CompileState::Ineligible => s.ineligible += 1,
                CompileState::Compiled(_) => s.compiled += 1,
            }
        }
        s
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub pending: usize,
    pub ineligible: usize,
    pub compiled: usize,
}

/// Try to unbox a `Vec<Value>` to a fixed-size `[i64]` matching
/// the JIT calling convention. Returns `None` if any arg isn't
/// `Int` / `Bool` or the count mismatches.
fn unbox_args(args: &[Value], expected_arity: u16) -> Option<Vec<i64>> {
    if args.len() != expected_arity as usize {
        return None;
    }
    let mut out = Vec::with_capacity(args.len());
    for v in args {
        match v {
            Value::Int(n) => out.push(*n),
            Value::Bool(b) => out.push(if *b { 1 } else { 0 }),
            _ => return None,
        }
    }
    Some(out)
}

