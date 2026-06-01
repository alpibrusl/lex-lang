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

/// Cheap structural check: does every op in this function belong
/// to the MVP-supported set? Callers should run this *before*
/// instantiating a [`JitContext`] — `compile` will return
/// [`JitError::UnsupportedOp`] for the first offender, but the
/// predicate lets you do the gate without building a module.
pub fn is_jit_eligible(f: &Function, consts: &[Const]) -> bool {
    if f.arity > 6 {
        return false;
    }
    for op in &f.code {
        if !op_supported(op, consts) {
            return false;
        }
    }
    true
}

pub(crate) fn op_supported(op: &Op, consts: &[Const]) -> bool {
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

#[cfg(feature = "cranelift")]
mod lower;

#[cfg(feature = "cranelift")]
pub use lower::{JitContext, JittedFn};

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
        pub unsafe fn call(&self, _args: &[i64]) -> i64 {
            unreachable!("no JittedFn can be constructed without the `cranelift` feature")
        }
        pub fn arity(&self) -> u16 { 0 }
    }
}

#[cfg(not(feature = "cranelift"))]
pub use stub::{JitContext, JittedFn};

