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
}

/// Host-side effect dispatch. Implementors decide what `kind`/`op` mean
/// and how arguments map to side effects.
pub trait EffectHandler {
    fn dispatch(&mut self, kind: &str, op: &str, args: Vec<Value>) -> Result<Value, String>;
}

/// A handler that fails any effect call. Useful as a default for pure-only runs.
pub struct DenyAllEffects;
impl EffectHandler for DenyAllEffects {
    fn dispatch(&mut self, kind: &str, op: &str, _args: Vec<Value>) -> Result<Value, String> {
        Err(format!("effects not permitted (attempted {kind}.{op})"))
    }
}

pub struct Vm<'a> {
    program: &'a Program,
    handler: Box<dyn EffectHandler + 'a>,
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
}

impl<'a> Vm<'a> {
    pub fn new(program: &'a Program) -> Self {
        Self::with_handler(program, Box::new(DenyAllEffects))
    }

    pub fn with_handler(program: &'a Program, handler: Box<dyn EffectHandler + 'a>) -> Self {
        Self {
            program,
            handler,
            frames: Vec::new(),
            stack: Vec::new(),
            step_limit: 10_000_000,
            steps: 0,
        }
    }

    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let fn_id = self.program.lookup(name).ok_or_else(|| VmError::Panic(format!("no function `{name}`")))?;
        self.invoke(fn_id, args)
    }

    pub fn invoke(&mut self, fn_id: u32, args: Vec<Value>) -> Result<Value, VmError> {
        let f = &self.program.functions[fn_id as usize];
        if args.len() != f.arity as usize {
            return Err(VmError::Panic(format!("arity mismatch calling {}", f.name)));
        }
        let mut locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
        for (i, v) in args.into_iter().enumerate() { locals[i] = v; }
        self.frames.push(Frame { fn_id, pc: 0, locals, stack_base: self.stack.len() });
        self.run()
    }

    fn run(&mut self) -> Result<Value, VmError> {
        loop {
            if self.steps > self.step_limit {
                return Err(VmError::Panic("step limit exceeded".into()));
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
                Op::Call { fn_id, arity } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let f = &self.program.functions[fn_id as usize];
                    let mut locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
                    for (i, v) in args.into_iter().enumerate() { locals[i] = v; }
                    self.frames.push(Frame { fn_id, pc: 0, locals, stack_base: self.stack.len() });
                }
                Op::TailCall { fn_id, arity } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    // Reuse current frame.
                    let f = &self.program.functions[fn_id as usize];
                    let frame = self.frames.last_mut().unwrap();
                    frame.fn_id = fn_id;
                    frame.pc = 0;
                    frame.locals = vec![Value::Unit; f.locals_count.max(f.arity) as usize];
                    for (i, v) in args.into_iter().enumerate() { frame.locals[i] = v; }
                    // Stack base stays the same.
                }
                Op::EffectCall { kind_idx, op_idx, arity } => {
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
                    let r = self.handler.dispatch(&kind, &op_name, args)
                        .map_err(VmError::Effect)?;
                    self.stack.push(r);
                }
                Op::Return => {
                    let v = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    // Trim any extra stuff that the function pushed but didn't pop.
                    self.stack.truncate(frame.stack_base);
                    if self.frames.is_empty() {
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
        Const::FieldName(s) | Const::VariantName(s) => Value::Str(s.clone()),
    }
}
