//! Cranelift-backed JIT for a subset of Lex bytecode.
//!
//! This is the #465 phase-1 MVP — a proof-of-concept that the
//! bytecode → CLIF → native-code path actually works end-to-end on
//! the simplest function shapes. It is **not** wired into the VM
//! dispatcher; the only entry point is [`JitContext::compile`],
//! which takes a `&Function` plus its constant pool and returns a
//! callable [`JittedFn`]. Tests compare the JITed result to the
//! interpreter on the same inputs.
//!
//! ## What is supported
//!
//! The op set covers integer arithmetic, boolean logic, locals,
//! straight-line control flow, and structured forward / backward
//! jumps — enough for tail-recursion-free numeric kernels:
//!
//! - `PushConst(i)` where `consts[i]` is `Int` or `Bool`
//! - `Pop`
//! - `LoadLocal(i)`, `StoreLocal(i)`
//! - `IntAdd`, `IntSub`, `IntMul`, `IntDiv`, `IntMod`, `IntNeg`
//! - `IntEq`, `IntLt`, `IntLe`
//! - `BoolAnd`, `BoolOr`, `BoolNot`
//! - `Jump(off)`, `JumpIf(off)`, `JumpIfNot(off)`
//! - `Return`
//!
//! Anything else makes the function ineligible — callers must check
//! [`is_jit_eligible`] before calling [`JitContext::compile`].
//!
//! ## Value representation
//!
//! Every Lex value at the JIT boundary is an `i64`. `Int` flows
//! through unchanged; `Bool` is encoded as `0` / `1`. This is the
//! whole point of the MVP — proving that on a constrained op set
//! we can drop the boxed `Value` enum and run on unboxed registers.
//! Extending the JIT to closures / records will require either
//! NaN-boxing (`#465` phase 2) or a deopt path back into the
//! interpreter for non-int values.
//!
//! ## Calling convention
//!
//! The JITed function takes `arity` `i64` arguments in declaration
//! order and returns one `i64`. Callers route through
//! [`JittedFn::call`], which dispatches via a fixed table of
//! `extern "C"` trampolines up to arity 6 — enough for the MVP
//! shape `fn arith(a, b, c, ...) -> Int`. Higher arities return an
//! error at compile time.
//!
//! Without the `cranelift` cargo feature this crate compiles to an
//! empty surface so that adding it to the workspace doesn't slow
//! down stable builds or pull cranelift into release artifacts.

#![cfg_attr(not(feature = "cranelift"), allow(dead_code))]

use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::Function;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum JitError {
    #[error("op {0:?} is not supported by the MVP JIT")]
    UnsupportedOp(Op),
    #[error("PushConst references an unsupported constant kind at index {0}")]
    UnsupportedConst(u32),
    #[error("function arity {0} exceeds the MVP trampoline table (max 6)")]
    ArityTooLarge(u16),
    #[error("jump offset out of range at pc {pc}: target {target}, code length {len}")]
    JumpOutOfRange { pc: usize, target: isize, len: usize },
    #[error("malformed bytecode: stack underflow at pc {0}")]
    StackUnderflow(usize),
    #[error("malformed bytecode: stack height mismatch at block entry pc {pc} ({existing} vs {seen})")]
    HeightMismatch { pc: usize, existing: u32, seen: u32 },
    #[error("function has no Return on a reachable path")]
    NoReturn,
    /// Boxed because `cranelift_module::ModuleError` is ~136 bytes,
    /// which would make every `Result<_, JitError>` carry a fat
    /// payload across the JIT boundary (clippy::result_large_err).
    #[cfg(feature = "cranelift")]
    #[error("cranelift module error: {0}")]
    Module(#[from] Box<cranelift_module::ModuleError>),
    /// Catch-all for backend errors and feature-gating messages.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Cheap structural check: is every op in this function in the
/// MVP-supported set? Self-recursive `Op::TailCall` is treated as a
/// supported op (lowered to a backward jump to the function entry);
/// `fn_id` is the index of `f` in its `Program.functions` so the
/// predicate can recognize self-references. Callers that hand-build
/// functions for testing can pass `0`.
pub fn is_jit_eligible(fn_id: u32, f: &Function, consts: &[Const]) -> bool {
    if f.arity > 6 {
        return false;
    }
    // Refinements run at the call boundary; bypassing them via JIT
    // would silently change observable behavior (a refinement
    // failure would no longer raise `VmError::RefinementFailed`).
    // The interpreter checks refinements *before* consulting the
    // hook, so by-the-letter-of-the-contract a refinement-bearing
    // function could be JITed safely — but we reject conservatively
    // to keep the eligibility predicate independent of where the
    // hook fires.
    if f.refinements.iter().any(|r| r.is_some()) {
        return false;
    }
    // Effects: the MVP op set excludes EffectCall, so any function
    // with declared effects could only be eligible by accident
    // (effects could still appear in tracer/handler interactions).
    // Reject to keep the contract simple.
    if !f.effects.is_empty() {
        return false;
    }
    for op in &f.code {
        if !op_supported_in(op, consts, fn_id) {
            return false;
        }
    }
    true
}

pub(crate) fn op_supported(op: &Op, consts: &[Const]) -> bool {
    // Standalone op check used by the lowering's per-op fallthrough.
    // The `op_supported_in` variant additionally accepts
    // self-recursive tail calls (which need the current fn_id).
    match op {
        Op::PushConst(i) => matches!(
            consts.get(*i as usize),
            Some(Const::Int(_)) | Some(Const::Bool(_))
        ),
        Op::Pop
        | Op::LoadLocal(_)
        | Op::StoreLocal(_)
        | Op::IntAdd
        | Op::IntSub
        | Op::IntMul
        | Op::IntDiv
        | Op::IntMod
        | Op::IntNeg
        | Op::IntEq
        | Op::IntLt
        | Op::IntLe
        | Op::BoolAnd
        | Op::BoolOr
        | Op::BoolNot
        | Op::Jump(_)
        | Op::JumpIf(_)
        | Op::JumpIfNot(_)
        | Op::Return => true,
        _ => false,
    }
}

pub(crate) fn op_supported_in(op: &Op, consts: &[Const], self_fn_id: u32) -> bool {
    // Self-recursive tail calls lower to a backward jump that
    // re-enters the function's entry block with new args.
    // Cross-function tail calls remain unsupported — they'd need
    // a different lowering (basically a call instruction) which
    // pulls all the cross-function caching back in.
    if let Op::TailCall { fn_id, .. } = op {
        return *fn_id == self_fn_id;
    }
    op_supported(op, consts)
}

#[cfg(feature = "cranelift")]
mod lower;
#[cfg(feature = "cranelift")]
pub mod tier;

#[cfg(feature = "cranelift")]
pub use lower::{JitContext, JittedFn};
#[cfg(feature = "cranelift")]
pub use tier::{CacheStats, JitTier, JitVm};

#[cfg(not(feature = "cranelift"))]
mod stub {
    use super::*;
    /// Stub when the `cranelift` feature is off — exposed so callers
    /// can still depend on `lex-jit` without the optional backend.
    pub struct JitContext;
    impl JitContext {
        pub fn new() -> Result<Self, JitError> {
            Err(JitError::Backend("lex-jit built without `cranelift` feature".into()))
        }
        pub fn compile(&mut self, _f: &Function, _consts: &[Const]) -> Result<JittedFn, JitError> {
            unreachable!("JitContext::new errors first")
        }
    }
    pub struct JittedFn;
    impl JittedFn {
        /// Mirror of the cranelift-feature `JittedFn::call` surface.
        ///
        /// # Safety
        ///
        /// Unconstructible without the `cranelift` feature — the
        /// surface exists only so callers can name the type. Any
        /// invocation panics via `unreachable!`.
        pub unsafe fn call(
            &self,
            _args: &[i64],
            _step_counter_ptr: *mut u64,
            _step_limit: u64,
            _aborted_out: *mut u8,
        ) -> i64 {
            unreachable!("no JittedFn can be constructed without the `cranelift` feature")
        }
        pub fn arity(&self) -> u16 { 0 }
    }
}

#[cfg(not(feature = "cranelift"))]
pub use stub::{JitContext, JittedFn};

