//! Tier-up integration.
//!
//! ## Architecture
//!
//! Two layers:
//!
//! - [`JitTier`] — the [`JitHook`] implementation. Owns a
//!   [`JitContext`] plus a per-function `CompileState` cache. Can
//!   be installed on any [`Vm`] via [`Vm::set_jit_hook`]; once
//!   installed, every `Op::Call` to an eligible function (and the
//!   outer `Vm::call` entry too) dispatches through native code.
//!
//! - [`JitVm`] — a convenience wrapper that constructs a `Vm` and
//!   a `JitTier` together, installs the latter on the former, and
//!   exposes a `call(name, args)` surface mirroring `Vm::call`.
//!   Callers that just want JIT on a fresh program use this; users
//!   with a pre-built `Vm` (custom effect handler, tracer, etc.)
//!   build a `JitTier` directly and call `Vm::set_jit_hook`.
//!
//! ## What this slice covers
//!
//! - `Op::Call` (in-program function calls).
//! - `Vm::call` / `Vm::invoke` (the public entry).
//!
//! ## What it does NOT cover
//!
//! - **`Op::TailCall`** — frame-replacement semantics need careful
//!   tracer + memo coordination; deferred to the next slice.
//! - **`Op::CallClosure`** — needs `body_hash` keyed cache (closure
//!   bodies don't have a `fn_id`); deferred.
//! - **Tier-down / deopt** — once `Ineligible`, terminal for the
//!   life of the tier.
//!
//! ## Cache state
//!
//! A function's `CompileState` transitions through:
//!
//! ```text
//!   Pending { counter: 0 }
//!         |
//!         | call N: counter += 1
//!         v
//!   Pending { counter >= threshold }
//!         |
//!         | attempt_compile
//!         |
//!         +---> Compiled(JittedFn) — native dispatch from here on
//!         |
//!         +---> Ineligible          — interpreter forever
//! ```
//!
//! The `Pending`/`Compiled`/`Ineligible` distinction lets us
//! report cache health via [`JitTier::cache_stats`] for tests and
//! benches without exposing the internal enum.

use lex_bytecode::jit_hook::JitHook;
use lex_bytecode::program::Program;
use lex_bytecode::value::Value;
use lex_bytecode::vm::{Vm, VmError};

use crate::{is_jit_eligible, JitContext, JitError, JittedFn};

/// JIT cache state for a single function.
enum CompileState {
    Pending { counter: u32 },
    Ineligible,
    Compiled(JittedFn),
}

/// JIT-hook implementation. Installable on any [`Vm`] via
/// [`Vm::set_jit_hook`]. Routes eligible calls through native code,
/// declines (returns `Ok(None)`) for everything else so the
/// interpreter handles them normally.
pub struct JitTier<'a> {
    ctx: JitContext,
    cache: Vec<CompileState>,
    threshold: u32,
    program: &'a Program,
}

impl<'a> JitTier<'a> {
    /// Build a tier over `program` with the default threshold (1 —
    /// eager: compile on first call). Use [`JitTier::with_threshold`]
    /// to defer.
    pub fn new(program: &'a Program) -> Result<Self, JitError> {
        Self::with_threshold(program, 1)
    }

    pub fn with_threshold(program: &'a Program, threshold: u32) -> Result<Self, JitError> {
        let cache = (0..program.functions.len())
            .map(|_| CompileState::Pending { counter: 0 })
            .collect();
        Ok(Self {
            ctx: JitContext::new()?,
            cache,
            threshold: threshold.max(1),
            program,
        })
    }

    /// How many functions are in each cache state right now.
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

    fn attempt_compile(&mut self, fn_id: usize) -> CompileState {
        let f = &self.program.functions[fn_id];
        if !is_jit_eligible(fn_id as u32, f, &self.program.constants) {
            return CompileState::Ineligible;
        }
        match self.ctx.compile(fn_id as u32, f, &self.program.constants) {
            Ok(jitted) => CompileState::Compiled(jitted),
            Err(_) => CompileState::Ineligible,
        }
    }
}

impl<'a> JitHook for JitTier<'a> {
    // `try_call` takes a raw pointer that we deref (via the
    // JITed code's load/store and our own `*step_counter_ptr`
    // read on abort). The trait itself is safe — see the
    // `JitHook` docs in `lex-bytecode`: the dispatcher is
    // responsible for providing a valid `&mut self.steps as *mut
    // u64` from the same `&mut self` borrow it just took. Marking
    // the trait `unsafe` would force every interpreter call site
    // to use `unsafe { hook.try_call(...) }`, which obscures the
    // common "no JIT installed" path. Allow + comment instead.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn try_call(
        &mut self,
        fn_id: u32,
        args: &[Value],
        step_counter_ptr: *mut u64,
        step_limit: u64,
    ) -> Result<Option<Value>, VmError> {
        let fid = fn_id as usize;
        if fid >= self.cache.len() {
            return Ok(None);
        }
        // Bump counter / maybe transition Pending → Compiled/Ineligible.
        if let CompileState::Pending { counter } = &mut self.cache[fid] {
            *counter += 1;
            if *counter >= self.threshold {
                self.cache[fid] = self.attempt_compile(fid);
            }
        }
        match &self.cache[fid] {
            CompileState::Compiled(jitted) => {
                if let Some(unboxed) = unbox_args(args, jitted.arity()) {
                    // Run the native function with the VM's step
                    // counter pointer; JITed code increments it at
                    // every backward jump and signals an abort via
                    // `aborted` if it would exceed `step_limit`.
                    let mut aborted: u8 = 0;
                    let r = unsafe {
                        jitted.call(&unboxed, step_counter_ptr, step_limit, &mut aborted)
                    };
                    if aborted != 0 {
                        // Surface the same error shape the
                        // interpreter would on `step_limit`: the
                        // dispatch loop raises `VmError::Panic`
                        // (it doesn't have a dedicated variant for
                        // step-limit). Match the interpreter's
                        // `step limit exceeded in <fn_name>` message
                        // so callers can grep either path the same
                        // way.
                        let fn_name = &self.program.functions[fid].name;
                        let count = unsafe { *step_counter_ptr };
                        return Err(VmError::Panic(format!(
                            "step limit exceeded in `{fn_name}` ({} > {}) [JIT]",
                            count, step_limit,
                        )));
                    }
                    Ok(Some(Value::Int(r)))
                } else {
                    // Eligible but arg shape doesn't fit at the call
                    // site — decline so the interpreter handles it.
                    Ok(None)
                }
            }
            CompileState::Ineligible | CompileState::Pending { .. } => Ok(None),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub pending: usize,
    pub ineligible: usize,
    pub compiled: usize,
}

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

// ---------------------------------------------------------------------------
// JitVm — convenience wrapper that pairs a fresh `Vm` with a `JitTier`.
// ---------------------------------------------------------------------------

/// Convenience: a `Vm` with a `JitTier` already installed. Users
/// who need a custom effect handler / tracer should build the
/// `Vm` themselves and call `Vm::set_jit_hook(Some(Box::new(
/// JitTier::new(program)?)))` directly.
///
/// The tier lives inside the `Vm`'s `jit_hook` slot, so cache
/// stats and other tier methods aren't directly reachable through
/// `JitVm`. If you need them, build the `Vm` + `JitTier` yourself
/// and keep a shared handle.
pub struct JitVm<'a> {
    vm: Vm<'a>,
}

impl<'a> JitVm<'a> {
    pub fn new(program: &'a Program) -> Result<Self, JitError> {
        Self::with_threshold(program, 1)
    }

    pub fn with_threshold(program: &'a Program, threshold: u32) -> Result<Self, JitError> {
        let tier = JitTier::with_threshold(program, threshold)?;
        let mut vm = Vm::new(program);
        vm.set_jit_hook(Some(Box::new(tier)));
        Ok(Self { vm })
    }

    pub fn vm_mut(&mut self) -> &mut Vm<'a> {
        &mut self.vm
    }

    /// Same shape as [`Vm::call`]. The JIT tier is installed as a
    /// hook on the underlying VM, so eligible top-level calls *and*
    /// internal `Op::Call`s are routed through native code.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        self.vm.call(name, args)
    }
}
