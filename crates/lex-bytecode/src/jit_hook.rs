//! JIT hook trait — the seam through which `lex-bytecode`'s
//! dispatch loop can delegate eligible `Op::Call` invocations to
//! a JIT tier without taking a compile-time dependency on the
//! JIT crate.
//!
//! ## Why a trait
//!
//! `lex-jit` already depends on `lex-bytecode` (for `Op`,
//! `Function`, `Value`, etc.), so `lex-bytecode` cannot in turn
//! depend on `lex-jit` directly. The trait inverts that: callers
//! that want JIT register a [`JitHook`] implementation on the
//! [`Vm`](crate::vm::Vm) at construction; the dispatch loop
//! consults the hook on each `Op::Call` and falls through to the
//! interpreter if it returns `Ok(None)`. No JIT in the build →
//! `vm.jit_hook` stays `None` and the hook check is one branch
//! on a null option (the optimizer should fold it).
//!
//! ## Step accounting (#465 architectural fix)
//!
//! `try_call` takes a raw pointer to the VM's step counter and
//! the limit value. JITed code is expected to increment the
//! counter at every backward jump it emits (loop headers, self-
//! recursive tail calls) and abort cleanly when the counter
//! reaches the limit — surfacing a `VmError::Panic("step limit
//! exceeded")` to the dispatcher.
//!
//! Without this contract, JITed native loops bypass the
//! interpreter's per-op step counter — the
//! `set_step_limit`-as-DoS-guard story the interpreter
//! documents would silently not hold under `--jit`. With it,
//! `--max-steps` is honored on both paths (modulo coarser
//! granularity in JIT: 1 step per loop iteration vs 1 step per
//! op for the interpreter; same wall-clock bound to within a
//! constant factor).
//!
//! The pointer is valid for the duration of the call — the
//! dispatcher passes `&mut self.steps as *mut u64` from the same
//! `&mut self` borrow it just took to invoke the hook. JITed
//! code dereferences and mutates it; on return the dispatcher
//! resumes seeing the updated value.
//!
//! ## Contract
//!
//! Implementations must be *observationally equivalent* to the
//! interpreter on the calls they accept:
//!
//! - **Effects.** Don't accept calls into effectful functions —
//!   the dispatcher doesn't route effect ops through the hook,
//!   so any effect call would be silently dropped.
//! - **Refinements.** The dispatch arm runs refinement checks
//!   *before* calling the hook (`Op::Call`'s existing path);
//!   hook implementors don't need to re-check them, but must
//!   decline (return `Ok(None)`) for functions whose refinement
//!   evaluation could change observable behavior of the call.
//!   The MVP JIT's eligibility predicate (`is_jit_eligible`)
//!   excludes any function with non-`None` refinements precisely
//!   for this reason.
//! - **Memoization.** The hook fires *after* the memo cache
//!   check, so a JIT call only happens on memo misses (or
//!   functions with memo disabled). This preserves the memo's
//!   observable behavior (same trace-event shape on a hit).
//! - **Tracing.** The dispatch arm emits `tracer.enter_call` for
//!   the call before invoking the hook; on a hook hit, the arm
//!   emits `tracer.exit_ok` itself. Hook implementors must not
//!   touch the tracer.

use crate::value::Value;
use crate::vm::VmError;

/// Hook into the VM dispatch loop for `Op::Call`.
///
/// See the module docs for the contract.
pub trait JitHook: Send {
    /// The dispatch loop has just verified refinements and missed
    /// the memo cache for `fn_id`. The arguments are at the top
    /// of the value stack — `args` is a borrowed view; do not
    /// mutate.
    ///
    /// `step_counter_ptr` points at the VM's `steps: u64` field;
    /// `step_limit` is the current cap. JITed code is expected
    /// to atomically `*step_counter_ptr += 1` at every backward
    /// jump (loop iteration) and abort if the new value would
    /// meet or exceed `step_limit`, surfacing
    /// `VmError::Panic("step limit exceeded")` so the dispatcher
    /// can propagate the same shape it would have on the
    /// interpreter path.
    ///
    /// Return:
    /// - `Ok(Some(v))` — hook handled the call; the dispatcher
    ///   will pop `args.len()` values from the stack, push `v`,
    ///   emit the synthetic `exit_ok` trace event, and continue.
    /// - `Ok(None)` — hook declines; the dispatcher proceeds with
    ///   normal frame setup as if the hook weren't installed.
    /// - `Err(e)` — JITed code raised an error (typically
    ///   `VmError::Panic("step limit exceeded")`). The dispatcher
    ///   surfaces it as the call's error.
    fn try_call(
        &mut self,
        fn_id: u32,
        args: &[Value],
        step_counter_ptr: *mut u64,
        step_limit: u64,
    ) -> Result<Option<Value>, VmError>;
}
