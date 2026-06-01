//! Bytecode → Cranelift IR lowering for the MVP op subset.
//!
//! Two passes:
//! 1. [`scan_blocks`] walks the op stream to find basic-block
//!    entries (pc 0, every jump target, every pc following a
//!    jump) and computes the abstract-interpretation stack height
//!    at each entry.
//! 2. [`Lowering::run`] walks ops sequentially, emitting CLIF
//!    into the current `Block`. Block-entry stack values are
//!    threaded through CLIF `block_params`; jumps pass the
//!    current SSA stack as block-call args.
//!
//! Locals are CLIF `Variable`s (one per slot). Cranelift's SSA
//! frontend handles φ-nodes for us via `use_var`/`def_var`, so
//! we don't need to surface locals through block params even
//! across joins.

use std::collections::BTreeMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    types, AbiParam, Block, BlockArg, InstBuilder, Value as ClifValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::Function;

use crate::{is_jit_eligible, JitError};

/// Holds the cranelift module so that JITed code stays mapped for
/// the lifetime of the context. Drop the context and any
/// [`JittedFn`] handed out becomes a dangling pointer — the borrow
/// checker enforces this through the `&JitContext` borrow.
pub struct JitContext {
    module: JITModule,
    ctx: Context,
    fbctx: FunctionBuilderContext,
    next_id: u64,
}

impl JitContext {
    pub fn new() -> Result<Self, JitError> {
        let mut flags = settings::builder();
        // opt_level=none keeps compile times bounded for the MVP;
        // baseline JIT (#465 phase 1) tunes this once we measure.
        flags.set("opt_level", "none")
            .map_err(|e| JitError::Backend(format!("flag opt_level: {e}")))?;
        flags.set("is_pic", "false")
            .map_err(|e| JitError::Backend(format!("flag is_pic: {e}")))?;
        let isa_builder = cranelift_native::builder()
            .map_err(|e| JitError::Backend(format!("native isa: {e}")))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .map_err(|e| JitError::Backend(format!("isa finish: {e}")))?;
        let jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let module = JITModule::new(jit_builder);
        let ctx = module.make_context();
        Ok(Self {
            module,
            ctx,
            fbctx: FunctionBuilderContext::new(),
            next_id: 0,
        })
    }

    /// Compile `f` against `consts`. Returns a callable
    /// [`JittedFn`] bound to the lifetime of `self`.
    pub fn compile(&mut self, f: &Function, consts: &[Const]) -> Result<JittedFn<'_>, JitError> {
        if !is_jit_eligible(f, consts) {
            // Surface the first offender for callers that didn't
            // pre-check, matching the doc-comment on JitContext::compile.
            if f.arity > 6 {
                return Err(JitError::ArityTooLarge(f.arity));
            }
            for op in &f.code {
                if !crate::op_supported(op, consts) {
                    if let Op::PushConst(i) = op {
                        return Err(JitError::UnsupportedConst(*i));
                    }
                    return Err(JitError::UnsupportedOp(*op));
                }
            }
            unreachable!("is_jit_eligible disagrees with op_supported");
        }

        // Reset and rebuild the function. `make_context` is only
        // run once at construction; subsequent compiles reuse the
        // Context after `clear` + signature setup.
        self.ctx.func.clear();
        self.ctx.func.signature.call_conv = CallConv::SystemV;
        for _ in 0..f.arity {
            self.ctx.func.signature.params.push(AbiParam::new(types::I64));
        }
        self.ctx.func.signature.returns.push(AbiParam::new(types::I64));

        Lowering::run(&mut self.ctx, &mut self.fbctx, f, consts)?;

        let name = format!("__lex_jit_{}", self.next_id);
        self.next_id += 1;
        let id = self
            .module
            .declare_function(&name, Linkage::Export, &self.ctx.func.signature)
            .map_err(Box::new)?;
        self.module
            .define_function(id, &mut self.ctx)
            .map_err(Box::new)?;
        self.module.clear_context(&mut self.ctx);
        self.module.finalize_definitions().map_err(Box::new)?;

        let code_ptr = self.module.get_finalized_function(id);
        Ok(JittedFn {
            code_ptr,
            arity: f.arity,
            _phantom: std::marker::PhantomData,
        })
    }
}

/// Handle to a JITed function. The code lives in the parent
/// [`JitContext`]'s `JITModule`; `'ctx` ties the lifetime so calling
/// after the context drops won't compile.
pub struct JittedFn<'ctx> {
    code_ptr: *const u8,
    arity: u16,
    _phantom: std::marker::PhantomData<&'ctx JitContext>,
}

impl<'ctx> JittedFn<'ctx> {
    pub fn arity(&self) -> u16 {
        self.arity
    }

    /// Invoke the JITed function. The caller must pass exactly
    /// `self.arity()` arguments, all encoded as `i64` (Bool → 0/1,
    /// Int → as-is). Returns the function's i64 result.
    ///
    /// # Safety
    ///
    /// - `args.len()` must equal `self.arity()` — otherwise we
    ///   transmute to the wrong fn signature and undefined behavior
    ///   follows.
    /// - The parent [`JitContext`] must still be alive (enforced
    ///   by the `'ctx` lifetime borrow at construction).
    pub unsafe fn call(&self, args: &[i64]) -> i64 {
        assert_eq!(args.len(), self.arity as usize, "JittedFn arity mismatch");
        let p = self.code_ptr;
        match self.arity {
            0 => {
                let f: extern "C" fn() -> i64 = std::mem::transmute(p);
                f()
            }
            1 => {
                let f: extern "C" fn(i64) -> i64 = std::mem::transmute(p);
                f(args[0])
            }
            2 => {
                let f: extern "C" fn(i64, i64) -> i64 = std::mem::transmute(p);
                f(args[0], args[1])
            }
            3 => {
                let f: extern "C" fn(i64, i64, i64) -> i64 = std::mem::transmute(p);
                f(args[0], args[1], args[2])
            }
            4 => {
                let f: extern "C" fn(i64, i64, i64, i64) -> i64 = std::mem::transmute(p);
                f(args[0], args[1], args[2], args[3])
            }
            5 => {
                let f: extern "C" fn(i64, i64, i64, i64, i64) -> i64 = std::mem::transmute(p);
                f(args[0], args[1], args[2], args[3], args[4])
            }
            6 => {
                let f: extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 =
                    std::mem::transmute(p);
                f(args[0], args[1], args[2], args[3], args[4], args[5])
            }
            n => unreachable!("arity {n} should have been rejected at compile time"),
        }
    }
}

// SAFETY: code_ptr points into the JITModule's executable mmap,
// which is `Send + Sync` for read-execute access.
unsafe impl<'ctx> Send for JittedFn<'ctx> {}
unsafe impl<'ctx> Sync for JittedFn<'ctx> {}

// ---------------------------------------------------------------------------
// Block-discovery pre-pass
// ---------------------------------------------------------------------------

/// Stack-height effect of an op on the value stack. `(pops, pushes)`.
/// Only meaningful for ops in the MVP subset.
fn op_height_delta(op: &Op) -> (u32, u32) {
    match op {
        Op::PushConst(_) | Op::LoadLocal(_) => (0, 1),
        Op::Pop | Op::StoreLocal(_) => (1, 0),
        Op::IntAdd | Op::IntSub | Op::IntMul | Op::IntDiv | Op::IntMod
        | Op::IntEq | Op::IntLt | Op::IntLe
        | Op::BoolAnd | Op::BoolOr => (2, 1),
        Op::IntNeg | Op::BoolNot => (1, 1),
        Op::Jump(_) => (0, 0),
        Op::JumpIf(_) | Op::JumpIfNot(_) => (1, 0),
        Op::Return => (1, 0),
        _ => (0, 0),
    }
}

fn is_terminator(op: &Op) -> bool {
    matches!(op, Op::Jump(_) | Op::Return)
}

/// Resolve a relative jump offset against the dispatch semantics
/// in `lex-bytecode`'s VM loop: `pc` is bumped to `pc + 1` *before*
/// the op runs, then `Jump(off)` does `pc = pc + off`. So the
/// branch target is `(pc_of_jump + 1) + off`.
fn jump_target(pc: usize, off: i32, code_len: usize) -> Result<usize, JitError> {
    let next = pc as isize + 1;
    let t = next + off as isize;
    if t < 0 || t as usize > code_len {
        return Err(JitError::JumpOutOfRange {
            pc,
            target: t,
            len: code_len,
        });
    }
    Ok(t as usize)
}

/// Scan the op stream to find block entries and the stack height
/// at each. Returns a map from entry-pc to entry-height.
///
/// Uses a worklist: seed with pc 0 (height 0), then for each block,
/// simulate forward op-by-op tracking height, recording every branch
/// target and post-jump pc as a new block entry. Heights at re-visits
/// are checked against the existing record.
fn scan_blocks(f: &Function) -> Result<BTreeMap<usize, u32>, JitError> {
    let code = &f.code;
    let mut entries: BTreeMap<usize, u32> = BTreeMap::new();
    let mut worklist: Vec<(usize, u32)> = vec![(0, 0)];

    while let Some((mut pc, mut height)) = worklist.pop() {
        // Re-visit check.
        if let Some(&existing) = entries.get(&pc) {
            if existing != height {
                return Err(JitError::HeightMismatch { pc, existing, seen: height });
            }
            continue;
        }
        entries.insert(pc, height);

        while pc < code.len() {
            let op = &code[pc];
            let (pops, pushes) = op_height_delta(op);
            if (height as i64) - (pops as i64) < 0 {
                return Err(JitError::StackUnderflow(pc));
            }
            let after_height = height - pops + pushes;

            match op {
                Op::Return => break,
                Op::Jump(off) => {
                    let t = jump_target(pc, *off, code.len())?;
                    worklist.push((t, after_height));
                    break;
                }
                Op::JumpIf(off) | Op::JumpIfNot(off) => {
                    let t = jump_target(pc, *off, code.len())?;
                    worklist.push((t, after_height));
                    let ft = pc + 1;
                    worklist.push((ft, after_height));
                    break;
                }
                _ => {
                    height = after_height;
                    pc += 1;
                }
            }
        }
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Main lowering walk
// ---------------------------------------------------------------------------

struct Lowering<'a> {
    builder: FunctionBuilder<'a>,
    blocks: BTreeMap<usize, (Block, u32)>,
    consts: &'a [Const],
    code: &'a [Op],
    stack: Vec<ClifValue>,
    /// Cranelift `Variable` handles, indexed by Lex local slot. The
    /// Lex compiler uses dense slot numbers `0..locals_count`, so a
    /// `Vec` indexed by slot suffices.
    locals: Vec<Variable>,
    /// Set true after a terminator; resets when we switch into the
    /// next block at its entry pc.
    terminated: bool,
}

fn values_to_block_args(vs: &[ClifValue]) -> Vec<BlockArg> {
    vs.iter().map(|v| BlockArg::from(*v)).collect()
}

impl<'a> Lowering<'a> {
    fn run(
        ctx: &mut Context,
        fbctx: &mut FunctionBuilderContext,
        f: &Function,
        consts: &[Const],
    ) -> Result<(), JitError> {
        let entries = scan_blocks(f)?;

        let mut builder = FunctionBuilder::new(&mut ctx.func, fbctx);

        // Pre-create all blocks; remember the height each enters at
        // so we can build the right number of block params.
        let mut blocks: BTreeMap<usize, (Block, u32)> = BTreeMap::new();
        for (&pc, &h) in &entries {
            let b = builder.create_block();
            for _ in 0..h {
                builder.append_block_param(b, types::I64);
            }
            blocks.insert(pc, (b, h));
        }

        // Entry block: the cranelift function entry has the user's
        // arguments as block params, but our entry block (pc=0) has
        // *value-stack* params (none, since height-at-entry is 0).
        // So we synthesize a separate cranelift-entry block that
        // grabs the function args, copies them into locals, then
        // jumps into the pc=0 block.
        let cf_entry = builder.create_block();
        builder.append_block_params_for_function_params(cf_entry);
        builder.switch_to_block(cf_entry);
        builder.seal_block(cf_entry);

        // Declare a Variable per local slot. SSA φ-nodes for locals
        // across joins are handled by cranelift's frontend.
        let arg_values: Vec<ClifValue> = builder.block_params(cf_entry).to_vec();
        let mut locals: Vec<Variable> = Vec::with_capacity(f.locals_count as usize);
        for i in 0..f.locals_count {
            let var = builder.declare_var(types::I64);
            if i < f.arity {
                builder.def_var(var, arg_values[i as usize]);
            } else {
                let zero = builder.ins().iconst(types::I64, 0);
                builder.def_var(var, zero);
            }
            locals.push(var);
        }
        let (pc0_block, pc0_height) = blocks[&0];
        debug_assert_eq!(pc0_height, 0, "pc 0 entry height must be 0");
        builder.ins().jump(pc0_block, &[]);

        let mut state = Lowering {
            builder,
            blocks,
            consts,
            code: &f.code,
            stack: Vec::new(),
            locals,
            terminated: true, // forces enter-block at pc 0
        };

        state.walk()?;

        state.builder.seal_all_blocks();
        state.builder.finalize();

        Ok(())
    }

    fn walk(&mut self) -> Result<(), JitError> {
        let n = self.code.len();
        let mut pc = 0;
        while pc < n {
            if let Some(&(block, height)) = self.blocks.get(&pc) {
                if !self.terminated {
                    // Falling through into a new block. Pass the
                    // current SSA stack as block-call args. The
                    // current stack height must match the target
                    // block's declared entry height — guaranteed by
                    // the abstract interp in scan_blocks.
                    debug_assert_eq!(self.stack.len() as u32, height);
                    let args = values_to_block_args(&self.stack);
                    self.builder.ins().jump(block, args.iter());
                }
                self.builder.switch_to_block(block);
                self.stack = self.builder.block_params(block).to_vec();
                self.terminated = false;
            } else if self.terminated {
                // Unreachable code between blocks. Skip until the
                // next block entry. (Could happen if the compiler
                // emits dead ops past a Jump.)
                pc += 1;
                continue;
            }

            let op = self.code[pc];
            self.emit_op(pc, op)?;

            if is_terminator(&op) || matches!(op, Op::JumpIf(_) | Op::JumpIfNot(_)) {
                // JumpIf/Not don't terminate the block in the
                // strict CFG sense (there is a fallthrough), but
                // emit_op took care of branching, so move on.
                pc += 1;
                continue;
            }
            pc += 1;
        }
        if !self.terminated {
            return Err(JitError::NoReturn);
        }
        Ok(())
    }

    fn pop(&mut self, pc: usize) -> Result<ClifValue, JitError> {
        self.stack.pop().ok_or(JitError::StackUnderflow(pc))
    }

    fn emit_op(&mut self, pc: usize, op: Op) -> Result<(), JitError> {
        match op {
            Op::PushConst(i) => {
                let c = self
                    .consts
                    .get(i as usize)
                    .ok_or(JitError::UnsupportedConst(i))?;
                let v = match c {
                    Const::Int(n) => self.builder.ins().iconst(types::I64, *n),
                    Const::Bool(b) => self.builder.ins().iconst(types::I64, if *b { 1 } else { 0 }),
                    _ => return Err(JitError::UnsupportedConst(i)),
                };
                self.stack.push(v);
            }
            Op::Pop => {
                self.pop(pc)?;
            }
            Op::LoadLocal(i) => {
                let var = self.locals[i as usize];
                let v = self.builder.use_var(var);
                self.stack.push(v);
            }
            Op::StoreLocal(i) => {
                let v = self.pop(pc)?;
                let var = self.locals[i as usize];
                self.builder.def_var(var, v);
            }
            Op::IntAdd => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().iadd(a, b));
            }
            Op::IntSub => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().isub(a, b));
            }
            Op::IntMul => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().imul(a, b));
            }
            Op::IntDiv => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().sdiv(a, b));
            }
            Op::IntMod => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().srem(a, b));
            }
            Op::IntNeg => {
                let a = self.pop(pc)?;
                let z = self.builder.ins().iconst(types::I64, 0);
                self.stack.push(self.builder.ins().isub(z, a));
            }
            Op::IntEq => self.emit_icmp(pc, IntCC::Equal)?,
            Op::IntLt => self.emit_icmp(pc, IntCC::SignedLessThan)?,
            Op::IntLe => self.emit_icmp(pc, IntCC::SignedLessThanOrEqual)?,
            Op::BoolAnd => {
                // Bools are 0/1; `iand` matches Rust semantics.
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().band(a, b));
            }
            Op::BoolOr => {
                let b = self.pop(pc)?;
                let a = self.pop(pc)?;
                self.stack.push(self.builder.ins().bor(a, b));
            }
            Op::BoolNot => {
                let a = self.pop(pc)?;
                let one = self.builder.ins().iconst(types::I64, 1);
                self.stack.push(self.builder.ins().bxor(a, one));
            }
            Op::Jump(off) => {
                let t = jump_target(pc, off, self.code.len())?;
                let (target_block, target_height) = self.blocks[&t];
                debug_assert_eq!(self.stack.len() as u32, target_height);
                let args = values_to_block_args(&self.stack);
                self.builder.ins().jump(target_block, args.iter());
                self.terminated = true;
            }
            Op::JumpIf(off) => self.emit_cond_jump(pc, off, true)?,
            Op::JumpIfNot(off) => self.emit_cond_jump(pc, off, false)?,
            Op::Return => {
                let v = self.pop(pc)?;
                self.builder.ins().return_(&[v]);
                self.terminated = true;
            }
            other => return Err(JitError::UnsupportedOp(other)),
        }
        Ok(())
    }

    fn emit_icmp(&mut self, pc: usize, cc: IntCC) -> Result<(), JitError> {
        let b = self.pop(pc)?;
        let a = self.pop(pc)?;
        let c = self.builder.ins().icmp(cc, a, b);
        // icmp produces I8; widen to I64 to match our 0/1 Bool encoding.
        let w = self.builder.ins().uextend(types::I64, c);
        self.stack.push(w);
        Ok(())
    }

    fn emit_cond_jump(&mut self, pc: usize, off: i32, jump_if_true: bool) -> Result<(), JitError> {
        let cond = self.pop(pc)?;
        let target = jump_target(pc, off, self.code.len())?;
        let ft = pc + 1;
        let (target_block, t_height) = self.blocks[&target];
        let (ft_block, ft_height) = self.blocks[&ft];
        debug_assert_eq!(self.stack.len() as u32, t_height);
        debug_assert_eq!(self.stack.len() as u32, ft_height);
        let args = values_to_block_args(&self.stack);
        let (then_b, else_b) = if jump_if_true {
            (target_block, ft_block)
        } else {
            (ft_block, target_block)
        };
        // brif args: (cond, then_block, then_args, else_block, else_args)
        self.builder
            .ins()
            .brif(cond, then_b, args.iter(), else_b, args.iter());
        self.terminated = true;
        Ok(())
    }
}
