//! Closure invocation from the host side (#221): `Vm` as a
//! `ClosureCaller` for the parser interpreter, and the
//! `invoke_closure_*` helpers that splice a closure's captures in
//! front of its call arguments before dispatching to the underlying
//! function. Reentrant: a closure may be invoked from inside an
//! already-running frame stack.

use super::*;

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

impl<'a> Vm<'a> {
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
}
