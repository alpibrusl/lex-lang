//! M5: bytecode VM. Stack machine with effect dispatch through a host handler.

use crate::op::*;
use crate::program::*;
use crate::value::Value;
use indexmap::IndexMap;

#[derive(Debug, Clone, thiserror::Error)]
pub enum VmError {
    #[error("runtime panic: {0}")]
    Panic(String),
    #[error("type mismatch at runtime: {0}")]
    TypeMismatch(String),
    #[error("stack underflow")]
    StackUnderflow,
    #[error("unknown function id: {0}")]
    UnknownFunction(u32),
    #[error("effect handler error: {0}")]
    Effect(String),
    #[error("call stack overflow: recursion depth exceeded ({0})")]
    CallStackOverflow(u32),
}

/// Maximum simultaneous call frames. Defends against unbounded
/// recursion in agent-emitted code: a body that calls itself
/// without a base case would otherwise blow the host's native
/// stack and crash the process. Real Lex code rarely exceeds
/// ~30 frames; 1024 is generous headroom while still well under
/// the OS stack limit at any per-frame size we use.
pub const MAX_CALL_DEPTH: u32 = 1024;

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
}

struct Frame {
    fn_id: u32,
    pc: usize,
    locals: Vec<Value>,
    /// Stack base when this frame started (for cleanup on return).
    stack_base: usize,
    trace_kind: FrameKind,
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
            frames: Vec::new(),
            stack: Vec::new(),
            step_limit: 10_000_000,
            steps: 0,
        }
    }

    pub fn set_tracer(&mut self, tracer: Box<dyn Tracer + 'a>) {
        self.tracer = tracer;
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
                let mut e = indexmap::IndexMap::new();
                e.insert("pos".into(), Value::Int(pos as i64));
                e.insert("message".into(), Value::Str(msg));
                Ok(Value::Variant {
                    name: "Err".into(),
                    args: vec![Value::Record(e)],
                })
            }
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

    pub fn invoke(&mut self, fn_id: u32, args: Vec<Value>) -> Result<Value, VmError> {
        let f = &self.program.functions[fn_id as usize];
        if args.len() != f.arity as usize {
            return Err(VmError::Panic(format!("arity mismatch calling {}", f.name)));
        }
        let mut locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
        for (i, v) in args.into_iter().enumerate() { locals[i] = v; }
        // Record the depth before pushing — this is what `run` will
        // exit at, supporting reentrant invocation from inside the
        // VM (e.g. the parser interpreter calling closures, #221).
        let base_depth = self.frames.len();
        self.push_frame(Frame {
            fn_id, pc: 0, locals, stack_base: self.stack.len(),
            trace_kind: FrameKind::Entry,
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
        loop {
            if self.steps > self.step_limit {
                return Err(VmError::Panic(format!(
                    "step limit exceeded ({} > {})",
                    self.steps, self.step_limit,
                )));
            }
            self.steps += 1;
            let frame_idx = self.frames.len() - 1;
            let pc = self.frames[frame_idx].pc;
            let fn_id = self.frames[frame_idx].fn_id;
            let code = &self.program.functions[fn_id as usize].code;
            if pc >= code.len() {
                return Err(VmError::Panic("ran past end of code".into()));
            }
            let op = code[pc].clone();
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
                    let v = self.frames[frame_idx].locals[i as usize].clone();
                    self.stack.push(v);
                }
                Op::StoreLocal(i) => {
                    let v = self.pop()?;
                    self.frames[frame_idx].locals[i as usize] = v;
                }
                Op::MakeRecord { field_name_indices } => {
                    let n = field_name_indices.len();
                    let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n).rev() {
                        values[i] = self.pop()?;
                    }
                    let mut rec = IndexMap::new();
                    for (i, val) in values.into_iter().enumerate() {
                        let name = match &self.program.constants[field_name_indices[i] as usize] {
                            Const::FieldName(s) => s.clone(),
                            _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                        };
                        rec.insert(name, val);
                    }
                    self.stack.push(Value::Record(rec));
                }
                Op::MakeTuple(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::Tuple(items));
                }
                Op::MakeList(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::List(items));
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
                Op::GetField(i) => {
                    let name = match &self.program.constants[i as usize] {
                        Const::FieldName(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                    };
                    let v = self.pop()?;
                    match v {
                        Value::Record(r) => {
                            let v = r.get(&name).cloned()
                                .ok_or_else(|| VmError::TypeMismatch(format!("missing field `{name}`")))?;
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetField on non-record: {other:?}"))),
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
                            items.push(value);
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
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let closure = self.pop()?;
                    let (fn_id, captures) = match closure {
                        Value::Closure { fn_id, captures, .. } => (fn_id, captures),
                        other => return Err(VmError::TypeMismatch(format!("CallClosure on non-closure: {other:?}"))),
                    };
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let callee_name = self.program.functions[fn_id as usize].name.clone();
                    let budget_cost = call_budget_cost(&self.program.functions[fn_id as usize]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    let mut combined = captures;
                    combined.extend(args);
                    self.tracer.enter_call(&node_id, &callee_name, &combined);
                    let f = &self.program.functions[fn_id as usize];
                    let mut locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
                    for (i, v) in combined.into_iter().enumerate() { locals[i] = v; }
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals, stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                    })?;
                }
                Op::Call { fn_id, arity, node_id_idx } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let callee_name = self.program.functions[fn_id as usize].name.clone();
                    let budget_cost = call_budget_cost(&self.program.functions[fn_id as usize]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    self.tracer.enter_call(&node_id, &callee_name, &args);
                    let f = &self.program.functions[fn_id as usize];
                    let mut locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
                    for (i, v) in args.into_iter().enumerate() { locals[i] = v; }
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals, stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                    })?;
                }
                Op::TailCall { fn_id, arity, node_id_idx } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let callee_name = self.program.functions[fn_id as usize].name.clone();
                    let budget_cost = call_budget_cost(&self.program.functions[fn_id as usize]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    // A tail call closes the current call's trace frame and
                    // opens a new one in its place — preserves the caller's
                    // tree depth in the trace.
                    self.tracer.exit_call_tail();
                    self.tracer.enter_call(&node_id, &callee_name, &args);
                    let f = &self.program.functions[fn_id as usize];
                    let frame = self.frames.last_mut().unwrap();
                    frame.fn_id = fn_id;
                    frame.pc = 0;
                    frame.locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
                    for (i, v) in args.into_iter().enumerate() { frame.locals[i] = v; }
                    frame.trace_kind = FrameKind::Call(node_id);
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
                            => self.run_parser_op(args.clone()),
                        None => self.handler.dispatch(&kind, &op_name, args.clone()),
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
                    if matches!(frame.trace_kind, FrameKind::Call(_)) {
                        self.tracer.exit_ok(&v);
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
                Op::NumAdd => self.bin_num(|a, b| Value::Int(a + b), |a, b| Value::Float(a + b))?,
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
                Op::NumLt => self.bin_num(|a, b| Value::Bool(a < b), |a, b| Value::Bool(a < b))?,
                Op::NumLe => self.bin_num(|a, b| Value::Bool(a <= b), |a, b| Value::Bool(a <= b))?,
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
                    self.stack.push(Value::Str(s));
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
            }
        }
    }

    fn pop(&mut self) -> Result<Value, VmError> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }
    fn peek(&self) -> Result<&Value, VmError> {
        self.stack.last().ok_or(VmError::StackUnderflow)
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
    fn bin_eq(&mut self) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.stack.push(Value::Bool(a == b));
        Ok(())
    }
}

fn const_to_value(c: &Const) -> Value {
    match c {
        Const::Int(n) => Value::Int(*n),
        Const::Float(f) => Value::Float(*f),
        Const::Bool(b) => Value::Bool(*b),
        Const::Str(s) => Value::Str(s.clone()),
        Const::Bytes(b) => Value::Bytes(b.clone()),
        Const::Unit => Value::Unit,
        Const::FieldName(s) | Const::VariantName(s) | Const::NodeId(s) => Value::Str(s.clone()),
    }
}
