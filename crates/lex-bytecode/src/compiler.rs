//! M4 compiler: canonical AST → bytecode.

use crate::op::*;
use crate::program::*;
use indexmap::IndexMap;
use lex_ast as a;

#[derive(Default)]
struct ConstPool {
    pool: Vec<Const>,
    fields: IndexMap<String, u32>,
    variants: IndexMap<String, u32>,
    node_ids: IndexMap<String, u32>,
    ints: IndexMap<i64, u32>,
    bools: IndexMap<u8, u32>,
    strs: IndexMap<String, u32>,
}

impl ConstPool {
    fn field(&mut self, name: &str) -> u32 {
        if let Some(i) = self.fields.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::FieldName(name.into()));
        self.fields.insert(name.into(), i);
        i
    }
    fn variant(&mut self, name: &str) -> u32 {
        if let Some(i) = self.variants.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::VariantName(name.into()));
        self.variants.insert(name.into(), i);
        i
    }
    fn node_id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.node_ids.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::NodeId(name.into()));
        self.node_ids.insert(name.into(), i);
        i
    }
    fn int(&mut self, n: i64) -> u32 {
        if let Some(i) = self.ints.get(&n) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Int(n));
        self.ints.insert(n, i);
        i
    }
    fn bool(&mut self, b: bool) -> u32 {
        let key = b as u8;
        if let Some(i) = self.bools.get(&key) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Bool(b));
        self.bools.insert(key, i);
        i
    }
    fn str(&mut self, s: &str) -> u32 {
        if let Some(i) = self.strs.get(s) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Str(s.into()));
        self.strs.insert(s.into(), i);
        i
    }
    fn float(&mut self, f: f64) -> u32 {
        // Floats: not deduped (NaN issues).
        let i = self.pool.len() as u32;
        self.pool.push(Const::Float(f));
        i
    }
    fn unit(&mut self) -> u32 {
        let i = self.pool.len() as u32;
        self.pool.push(Const::Unit);
        i
    }
}

pub fn compile_program(stages: &[a::Stage]) -> Program {
    let mut p = Program {
        constants: Vec::new(),
        functions: Vec::new(),
        function_names: IndexMap::new(),
        module_aliases: IndexMap::new(),
        entry: None,
    };

    // Collect imports as alias → module-name. The module name is the part
    // after `std.` (so `import "std.io" as io` ⇒ alias `io` → module `io`).
    for s in stages {
        if let a::Stage::Import(i) = s {
            let module = i.reference.strip_prefix("std.").unwrap_or(&i.reference).to_string();
            p.module_aliases.insert(i.alias.clone(), module);
        }
    }

    for s in stages {
        if let a::Stage::FnDecl(fd) = s {
            let idx = p.functions.len() as u32;
            p.function_names.insert(fd.name.clone(), idx);
            p.functions.push(Function {
                name: fd.name.clone(),
                arity: fd.params.len() as u16,
                locals_count: 0,
                code: Vec::new(),
                effects: fd.effects.iter().map(|e| DeclaredEffect {
                    kind: e.name.clone(),
                    arg: e.arg.as_ref().map(|a| match a {
                        a::EffectArg::Str { value } => EffectArg::Str(value.clone()),
                        a::EffectArg::Int { value } => EffectArg::Int(*value),
                        a::EffectArg::Ident { value } => EffectArg::Ident(value.clone()),
                    }),
                }).collect(),
                // Filled in at the end of the compile pass, once `code`
                // and `locals_count` are final. See #222.
                body_hash: crate::program::ZERO_BODY_HASH,
                // Per-param refinement predicates for runtime check
                // (#209 slice 3). Lifted directly from each param's
                // `TypeExpr::Refined` if present; `None` otherwise.
                refinements: fd.params.iter().map(|p| match &p.ty {
                    a::TypeExpr::Refined { binding, predicate, .. } =>
                        Some(crate::program::Refinement {
                            binding: binding.clone(),
                            predicate: (**predicate).clone(),
                        }),
                    _ => None,
                }).collect(),
            });
        }
    }

    let mut pool = ConstPool::default();
    let function_names = p.function_names.clone();
    let module_aliases = p.module_aliases.clone();
    let mut pending_lambdas: Vec<PendingLambda> = Vec::new();

    for s in stages {
        if let a::Stage::FnDecl(_) = s {
            // Build a NodeId map for *this* stage so the compiler can stamp
            // each Call/EffectCall opcode with the originating AST node.
            let id_map = lex_ast::expr_ids(s);
            let fd = match s { a::Stage::FnDecl(fd) => fd, _ => unreachable!() };
            let mut fc = FnCompiler {
                code: Vec::new(),
                locals: IndexMap::new(),
                next_local: 0,
                peak_local: 0,
                pool: &mut pool,
                function_names: &function_names,
                module_aliases: &module_aliases,
                id_map: &id_map,
                pending_lambdas: &mut pending_lambdas,
                next_fn_id: &mut p.functions,
            };
            for param in &fd.params {
                let i = fc.next_local;
                fc.locals.insert(param.name.clone(), i);
                fc.next_local += 1;
                fc.peak_local = fc.next_local;
            }
            fc.compile_expr(&fd.body, true);
            fc.code.push(Op::Return);
            let code = std::mem::take(&mut fc.code);
            let peak = fc.peak_local;
            drop(fc);
            let idx = function_names[&fd.name];
            p.functions[idx as usize].code = code;
            p.functions[idx as usize].locals_count = peak;
        }
    }

    // Compile pending lambdas in FIFO order. Each lambda may emit further
    // lambdas; loop until the queue drains.
    while let Some(pl) = pending_lambdas.pop() {
        let id_map = std::collections::HashMap::new();
        let mut fc = FnCompiler {
            code: Vec::new(),
            locals: IndexMap::new(),
            next_local: 0,
            peak_local: 0,
            pool: &mut pool,
            function_names: &function_names,
            module_aliases: &module_aliases,
            id_map: &id_map,
            pending_lambdas: &mut pending_lambdas,
            next_fn_id: &mut p.functions,
        };
        for name in &pl.capture_names {
            let i = fc.next_local;
            fc.locals.insert(name.clone(), i);
            fc.next_local += 1;
            fc.peak_local = fc.next_local;
        }
        for p in &pl.params {
            let i = fc.next_local;
            fc.locals.insert(p.name.clone(), i);
            fc.next_local += 1;
            fc.peak_local = fc.next_local;
        }
        fc.compile_expr(&pl.body, true);
        fc.code.push(Op::Return);
        let code = std::mem::take(&mut fc.code);
        let peak = fc.peak_local;
        drop(fc);
        p.functions[pl.fn_id as usize].code = code;
        p.functions[pl.fn_id as usize].locals_count = peak;
    }

    // Final pass: stamp every function with its content hash now that
    // every body is finalized (#222). Trampolines installed via
    // `install_trampoline` already have it; recomputing is cheap and
    // makes the invariant easier to read at this top level.
    for f in p.functions.iter_mut() {
        if f.body_hash == crate::program::ZERO_BODY_HASH {
            f.body_hash = crate::program::compute_body_hash(
                f.arity, f.locals_count, &f.code);
        }
    }

    p.constants = pool.pool;
    p
}

#[derive(Debug, Clone)]
struct PendingLambda {
    fn_id: u32,
    /// Names of captured outer-scope locals, in order.
    capture_names: Vec<String>,
    params: Vec<a::Param>,
    body: a::CExpr,
}

struct FnCompiler<'a> {
    code: Vec<Op>,
    locals: IndexMap<String, u16>,
    next_local: u16,
    /// Peak local usage seen during compilation (for VM frame sizing).
    peak_local: u16,
    pool: &'a mut ConstPool,
    function_names: &'a IndexMap<String, u32>,
    module_aliases: &'a IndexMap<String, String>,
    /// CExpr address → NodeId, populated per stage via `lex_ast::expr_ids`.
    id_map: &'a std::collections::HashMap<*const a::CExpr, lex_ast::NodeId>,
    /// Queue of lambdas discovered during compilation; each gets a fresh
    /// fn_id and is compiled in a later pass.
    pending_lambdas: &'a mut Vec<PendingLambda>,
    /// Mutable view of the function table — used to allocate fn_ids for
    /// freshly-discovered lambdas.
    next_fn_id: &'a mut Vec<Function>,
}

impl<'a> FnCompiler<'a> {
    fn alloc_local(&mut self, name: &str) -> u16 {
        let i = self.next_local;
        self.locals.insert(name.into(), i);
        self.next_local += 1;
        if self.next_local > self.peak_local { self.peak_local = self.next_local; }
        i
    }
    fn emit(&mut self, op: Op) { self.code.push(op); }

    fn compile_expr(&mut self, e: &a::CExpr, tail: bool) {
        match e {
            a::CExpr::Literal { value } => self.compile_lit(value),
            a::CExpr::Var { name } => {
                if let Some(slot) = self.locals.get(name) {
                    self.emit(Op::LoadLocal(*slot));
                } else if let Some(&fn_id) = self.function_names.get(name) {
                    // Function name used as a *value* (e.g. as a record-field
                    // initializer or fold-callback arg) — materialize it as a
                    // closure with no captures. The runtime already accepts
                    // `Value::Closure { fn_id, captures: vec![] }` and
                    // `CallClosure` dispatches it. (#169)
                    self.emit(Op::MakeClosure { fn_id, capture_count: 0 });
                } else {
                    // Should be caught at type-check time; the type checker
                    // walks every Var. If we land here it's a compiler bug,
                    // not a user typo.
                    panic!("unknown var in compiler: {name}");
                }
            }
            a::CExpr::Let { name, ty: _, value, body } => {
                self.compile_expr(value, false);
                let slot = self.alloc_local(name);
                self.emit(Op::StoreLocal(slot));
                self.compile_expr(body, tail);
            }
            a::CExpr::Block { statements, result } => {
                for s in statements {
                    self.compile_expr(s, false);
                    self.emit(Op::Pop);
                }
                self.compile_expr(result, tail);
            }
            a::CExpr::Call { callee, args } => self.compile_call(e, callee, args, tail),
            a::CExpr::Constructor { name, args } => {
                for a in args { self.compile_expr(a, false); }
                let name_idx = self.pool.variant(name);
                self.emit(Op::MakeVariant { name_idx, arity: args.len() as u16 });
            }
            a::CExpr::Match { scrutinee, arms } => self.compile_match(scrutinee, arms, tail),
            a::CExpr::RecordLit { fields } => {
                let mut idxs = Vec::with_capacity(fields.len());
                for f in fields {
                    self.compile_expr(&f.value, false);
                    idxs.push(self.pool.field(&f.name));
                }
                self.emit(Op::MakeRecord { field_name_indices: idxs });
            }
            a::CExpr::TupleLit { items } => {
                for it in items { self.compile_expr(it, false); }
                self.emit(Op::MakeTuple(items.len() as u16));
            }
            a::CExpr::ListLit { items } => {
                for it in items { self.compile_expr(it, false); }
                self.emit(Op::MakeList(items.len() as u32));
            }
            a::CExpr::FieldAccess { value, field } => {
                self.compile_expr(value, false);
                let idx = self.pool.field(field);
                self.emit(Op::GetField(idx));
            }
            a::CExpr::BinOp { op, lhs, rhs } => self.compile_binop(op, lhs, rhs),
            a::CExpr::UnaryOp { op, expr } => {
                self.compile_expr(expr, false);
                match op.as_str() {
                    "-" => self.emit(Op::NumNeg),
                    "not" => self.emit(Op::BoolNot),
                    other => panic!("unknown unary: {other}"),
                }
            }
            a::CExpr::Lambda { params, body, .. } => self.compile_lambda(params, body),
            a::CExpr::Return { value } => {
                self.compile_expr(value, true);
                self.emit(Op::Return);
            }
        }
    }

    fn compile_lit(&mut self, l: &a::CLit) {
        let i = match l {
            a::CLit::Int { value } => self.pool.int(*value),
            a::CLit::Bool { value } => self.pool.bool(*value),
            a::CLit::Float { value } => {
                let f: f64 = value.parse().unwrap_or(0.0);
                self.pool.float(f)
            }
            a::CLit::Str { value } => self.pool.str(value),
            a::CLit::Bytes { value: _ } => {
                // Stub: M4 doesn't use bytes literals in §3.13 examples.
                let i = self.pool.pool.len() as u32;
                self.pool.pool.push(Const::Bytes(Vec::new()));
                i
            }
            a::CLit::Unit => self.pool.unit(),
        };
        self.emit(Op::PushConst(i));
    }

    fn compile_call(&mut self, call_expr: &a::CExpr, callee: &a::CExpr, args: &[a::CExpr], tail: bool) {
        let node_id = self
            .id_map
            .get(&(call_expr as *const a::CExpr))
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "n_?".into());
        let node_id_idx = self.pool.node_id(&node_id);

        // Module function call: `alias.op(args)` where `alias` is an imported
        // module ⇒ EffectCall, except for higher-order pure ops where we
        // emit inline bytecode using CallClosure (the closure-arg can't be
        // serialized through the effect handler).
        if let a::CExpr::FieldAccess { value, field } = callee {
            if let a::CExpr::Var { name } = value.as_ref() {
                if let Some(module) = self.module_aliases.get(name) {
                    if self.try_emit_higher_order(module, field, args, node_id_idx) {
                        let _ = tail;
                        return;
                    }
                    for a in args { self.compile_expr(a, false); }
                    let kind_idx = self.pool.str(module);
                    let op_idx = self.pool.str(field);
                    self.emit(Op::EffectCall {
                        kind_idx,
                        op_idx,
                        arity: args.len() as u16,
                        node_id_idx,
                    });
                    let _ = tail;
                    return;
                }
            }
        }
        match callee {
            a::CExpr::Var { name } if self.function_names.contains_key(name) => {
                for a in args { self.compile_expr(a, false); }
                let fn_id = self.function_names[name];
                if tail {
                    self.emit(Op::TailCall { fn_id, arity: args.len() as u16, node_id_idx });
                } else {
                    self.emit(Op::Call { fn_id, arity: args.len() as u16, node_id_idx });
                }
            }
            a::CExpr::Var { name } if self.locals.contains_key(name) => {
                // First-class function value bound to a local. Push the
                // closure, then args, then CallClosure.
                let slot = self.locals[name];
                self.emit(Op::LoadLocal(slot));
                for a in args { self.compile_expr(a, false); }
                self.emit(Op::CallClosure { arity: args.len() as u16, node_id_idx });
            }
            // Lambda directly applied — push closure + args + CallClosure.
            other => {
                self.compile_expr(other, false);
                for a in args { self.compile_expr(a, false); }
                self.emit(Op::CallClosure { arity: args.len() as u16, node_id_idx });
            }
        }
    }

    fn compile_binop(&mut self, op: &str, lhs: &a::CExpr, rhs: &a::CExpr) {
        self.compile_expr(lhs, false);
        self.compile_expr(rhs, false);
        match op {
            "+" => self.emit(Op::NumAdd),
            "-" => self.emit(Op::NumSub),
            "*" => self.emit(Op::NumMul),
            "/" => self.emit(Op::NumDiv),
            "%" => self.emit(Op::NumMod),
            "==" => self.emit(Op::NumEq),
            "!=" => { self.emit(Op::NumEq); self.emit(Op::BoolNot); }
            "<" => self.emit(Op::NumLt),
            "<=" => self.emit(Op::NumLe),
            ">" => { self.emit_swap_top2(); self.emit(Op::NumLt); }
            ">=" => { self.emit_swap_top2(); self.emit(Op::NumLe); }
            "and" => self.emit(Op::BoolAnd),
            "or" => self.emit(Op::BoolOr),
            other => panic!("unknown binop: {other:?}"),
        }
    }

    fn emit_swap_top2(&mut self) {
        let a = self.alloc_local("__swap_a");
        let b = self.alloc_local("__swap_b");
        self.emit(Op::StoreLocal(b));
        self.emit(Op::StoreLocal(a));
        self.emit(Op::LoadLocal(b));
        self.emit(Op::LoadLocal(a));
    }

    fn compile_match(&mut self, scrutinee: &a::CExpr, arms: &[a::Arm], tail: bool) {
        self.compile_expr(scrutinee, false);
        let scrut_slot = self.alloc_local("__scrut");
        self.emit(Op::StoreLocal(scrut_slot));

        let mut end_jumps: Vec<usize> = Vec::new();
        for arm in arms {
            let arm_start_locals = self.next_local;
            let arm_start_locals_map = self.locals.clone();

            self.emit(Op::LoadLocal(scrut_slot));
            let mut bindings: Vec<(String, u16)> = Vec::new();
            let fail_jumps: Vec<usize> = self.compile_pattern_test(&arm.pattern, &mut bindings);

            self.compile_expr(&arm.body, tail);
            let j_end = self.code.len();
            self.emit(Op::Jump(0));
            end_jumps.push(j_end);

            let fail_target = self.code.len() as i32;
            for j in fail_jumps {
                // #337: PConstructor patterns now register an
                // unconditional `Op::Jump` for the failure path
                // (alongside the existing `Op::JumpIfNot` from
                // PLiteral / nested constructor tests). Patch
                // either shape.
                match &mut self.code[j] {
                    Op::JumpIfNot(off) => *off = fail_target - (j as i32 + 1),
                    Op::Jump(off)      => *off = fail_target - (j as i32 + 1),
                    _ => {}
                }
            }
            self.next_local = arm_start_locals;
            self.locals = arm_start_locals_map;
        }
        let panic_msg_idx = self.pool.str("non-exhaustive match");
        self.emit(Op::Panic(panic_msg_idx));

        let end_target = self.code.len() as i32;
        for j in end_jumps {
            if let Op::Jump(off) = &mut self.code[j] {
                *off = end_target - (j as i32 + 1);
            }
        }
    }

    fn compile_pattern_test(&mut self, p: &a::Pattern, bindings: &mut Vec<(String, u16)>) -> Vec<usize> {
        let mut fails = Vec::new();
        match p {
            a::Pattern::PWild => { self.emit(Op::Pop); }
            a::Pattern::PVar { name } => {
                let slot = self.alloc_local(name);
                self.emit(Op::StoreLocal(slot));
                bindings.push((name.clone(), slot));
            }
            a::Pattern::PLiteral { value } => {
                self.compile_lit(value);
                match value {
                    a::CLit::Str { .. } => self.emit(Op::StrEq),
                    a::CLit::Bytes { .. } => self.emit(Op::BytesEq),
                    _ => self.emit(Op::NumEq),
                }
                let j = self.code.len();
                self.emit(Op::JumpIfNot(0));
                fails.push(j);
            }
            a::Pattern::PConstructor { name, args } => {
                let name_idx = self.pool.variant(name);
                // #337: the failure path must drop the duplicated
                // scrutinee so subsequent match arms see a clean
                // stack. The previous shape
                //   Dup; TestVariant; JumpIfNot(fail);
                // left `[scrut]` on the stack at the fail target,
                // poisoning later arms — e.g. a wildcard `_` arm
                // whose body referenced an unrelated value would
                // pop the leaked scrutinee instead of its own value.
                //
                // New shape: branch on success, fall through to a
                // failure cleanup that pops the dup'd scrutinee
                // before jumping. The registered fail-jump is an
                // unconditional `Op::Jump`; `compile_match`'s patch
                // loop accepts both `JumpIfNot` and `Jump`.
                self.emit(Op::Dup);                   // [scrut, scrut]
                self.emit(Op::TestVariant(name_idx)); // [scrut, Bool]
                let j_success = self.code.len();
                self.emit(Op::JumpIf(0));             // pop Bool. success → [scrut]
                self.emit(Op::Pop);                   // failure cleanup: [scrut] → []
                let j_fail = self.code.len();
                self.emit(Op::Jump(0));               // → fail target with []
                fails.push(j_fail);
                let success_target = self.code.len() as i32;
                if let Op::JumpIf(off) = &mut self.code[j_success] {
                    *off = success_target - (j_success as i32 + 1);
                }
                if args.is_empty() {
                    self.emit(Op::Pop);
                } else if args.len() == 1 {
                    self.emit(Op::GetVariantArg(0));
                    let sub_fails = self.compile_pattern_test(&args[0], bindings);
                    fails.extend(sub_fails);
                } else {
                    let slot = self.alloc_local("__variant");
                    self.emit(Op::StoreLocal(slot));
                    for (i, arg) in args.iter().enumerate() {
                        self.emit(Op::LoadLocal(slot));
                        self.emit(Op::GetVariantArg(i as u16));
                        let sub_fails = self.compile_pattern_test(arg, bindings);
                        fails.extend(sub_fails);
                    }
                }
            }
            a::Pattern::PRecord { fields } => {
                let slot = self.alloc_local("__record");
                self.emit(Op::StoreLocal(slot));
                for f in fields {
                    self.emit(Op::LoadLocal(slot));
                    let fi = self.pool.field(&f.name);
                    self.emit(Op::GetField(fi));
                    let sub_fails = self.compile_pattern_test(&f.pattern, bindings);
                    fails.extend(sub_fails);
                }
            }
            a::Pattern::PTuple { items } => {
                let slot = self.alloc_local("__tuple");
                self.emit(Op::StoreLocal(slot));
                for (i, item) in items.iter().enumerate() {
                    self.emit(Op::LoadLocal(slot));
                    self.emit(Op::GetElem(i as u16));
                    let sub_fails = self.compile_pattern_test(item, bindings);
                    fails.extend(sub_fails);
                }
            }
        }
        fails
    }

    /// Compile a Lambda: collect free variables that resolve to outer-scope
    /// locals, register a synthetic function, emit MakeClosure with the
    /// captured values pushed in order.
    fn compile_lambda(&mut self, params: &[a::Param], body: &a::CExpr) {
        // Free vars = vars referenced in body that aren't bound locally.
        let mut bound: std::collections::HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut frees: Vec<String> = Vec::new();
        free_vars(body, &mut bound, &mut frees);

        // Filter to those that are in the enclosing locals (captures).
        // Don't exclude names that *also* exist in `function_names`:
        // if the name is in `locals`, the local shadows the global
        // within this scope, and the lambda needs to capture the
        // local's value, not the global fn. (#339) Names that are
        // ONLY in `function_names` (no local) stay external — the
        // lambda's body resolves them at call time, same as the
        // enclosing fn would.
        let captures: Vec<String> = frees.into_iter()
            .filter(|n| self.locals.contains_key(n))
            .collect();

        // Allocate a fresh fn_id by appending a placeholder Function.
        let fn_id = self.next_fn_id.len() as u32;
        self.next_fn_id.push(Function {
            name: format!("__lambda_{fn_id}"),
            arity: (captures.len() + params.len()) as u16,
            locals_count: 0,
            code: Vec::new(),
            effects: Vec::new(),
            // See #222: filled in at the end of the compile pass.
            body_hash: crate::program::ZERO_BODY_HASH,
            // Lambdas don't carry refinements at the surface today
            // (closure params don't accept `Type{x | ...}` syntax in
            // the parser). #209 stays focused on top-level fn decls;
            // closure-param refinements are a follow-up.
            refinements: Vec::new(),
        });

        // Emit code at the lambda site: load each captured local, then MakeClosure.
        for c in &captures {
            let slot = *self.locals.get(c).expect("free var must be in scope");
            self.emit(Op::LoadLocal(slot));
        }
        self.emit(Op::MakeClosure { fn_id, capture_count: captures.len() as u16 });

        // Queue the body for later compilation.
        self.pending_lambdas.push(PendingLambda {
            fn_id,
            capture_names: captures,
            params: params.to_vec(),
            body: body.clone(),
        });
    }

    /// Higher-order stdlib ops on Result/Option whose function arg is a
    /// closure. Emit inline: pattern-match on the variant, invoke the
    /// closure when applicable, return wrapped result.
    fn try_emit_higher_order(
        &mut self,
        module: &str,
        op: &str,
        args: &[a::CExpr],
        node_id_idx: u32,
    ) -> bool {
        match (module, op) {
            ("result", "map") => self.emit_variant_map(args, "Ok", true),
            ("result", "and_then") => self.emit_variant_map(args, "Ok", false),
            ("result", "map_err") => self.emit_variant_map(args, "Err", true),
            ("result", "or_else") => self.emit_variant_or_else(args, "Err", 1),
            ("option", "map") => self.emit_variant_map(args, "Some", true),
            ("option", "and_then") => self.emit_variant_map(args, "Some", false),
            ("option", "or_else") => self.emit_variant_or_else(args, "None", 0),
            ("option", "unwrap_or_else") => self.emit_option_unwrap_or_else(args),
            ("list", "map") => self.emit_list_map(args),
            ("list", "par_map") => self.emit_list_par_map(args),
            ("list", "sort_by") => self.emit_list_sort_by(args),
            ("list", "filter") => self.emit_list_filter(args),
            ("list", "fold") => self.emit_list_fold(args),
            ("iter", "from_list") => self.emit_iter_from_list(args),
            ("iter", "unfold")    => self.emit_iter_unfold(args),
            ("iter", "next")      => self.emit_iter_next(args),
            ("iter", "is_empty")  => self.emit_iter_is_empty(args),
            ("iter", "count")     => self.emit_iter_count(args),
            ("iter", "take")      => self.emit_iter_take(args),
            ("iter", "skip")      => self.emit_iter_skip(args),
            ("iter", "to_list")   => self.emit_iter_to_list(args),
            ("iter", "map")       => self.emit_iter_map(args),
            ("iter", "filter")    => self.emit_iter_filter(args),
            ("iter", "fold")      => self.emit_iter_fold(args),
            ("map", "fold") => self.emit_map_fold(args, node_id_idx),
            ("flow", "sequential") => self.emit_flow_sequential(args),
            ("flow", "branch") => self.emit_flow_branch(args),
            ("flow", "retry") => self.emit_flow_retry(args),
            ("flow", "retry_with_backoff") => self.emit_flow_retry_with_backoff(args),
            ("flow", "parallel") => self.emit_flow_parallel(args),
            ("flow", "parallel_list") => self.emit_flow_parallel_list(args),
            _ => return false,
        }
        true
    }

    /// `list.map(xs, f)` — inline loop applying `f` to each element.
    /// Stack contract: pushes the resulting List.
    fn emit_list_map(&mut self, args: &[a::CExpr]) {
        // Compile xs and f, store both as locals.
        self.compile_expr(&args[0], false);
        let xs = self.alloc_local("__lm_xs");
        self.emit(Op::StoreLocal(xs));
        self.compile_expr(&args[1], false);
        let f = self.alloc_local("__lm_f");
        self.emit(Op::StoreLocal(f));

        // out := []
        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__lm_out");
        self.emit(Op::StoreLocal(out));

        // i := 0
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__lm_i");
        self.emit(Op::StoreLocal(i));

        // loop_top: while i < len(xs) { ... }
        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // body: out := out ++ [f(xs[i])]
        let nid = self.pool.node_id("n_list_map");
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        // jump back
        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        // exit: patch j_exit, push out
        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `list.par_map(xs, f)` (#305 slice 1). Pushes `xs` and `f`,
    /// then emits a single `Op::ParallelMap` — the VM applies `f`
    /// to each element on OS-thread tasks, capped by
    /// `LEX_PAR_MAX_CONCURRENCY`. Returns the result list in input
    /// order.
    fn emit_list_par_map(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_list_par_map");
        self.emit(Op::ParallelMap { node_id_idx: nid });
    }

    /// `list.sort_by(xs, f)` (#338). Pushes `xs` and the key-fn
    /// `f`, then emits a single `Op::SortByKey` — the VM invokes
    /// `f` on each element to derive a sortable key, stable-sorts
    /// by key, and returns the values in sorted order. Keys must
    /// resolve to `Int` / `Float` / `Str`; mixed-type pairs are
    /// treated as equal by the comparator (preserving insertion
    /// order via the stable sort).
    fn emit_list_sort_by(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_list_sort_by");
        self.emit(Op::SortByKey { node_id_idx: nid });
    }

    /// `list.filter(xs, pred)` — keep elements where pred returns true.
    fn emit_list_filter(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let xs = self.alloc_local("__lf_xs");
        self.emit(Op::StoreLocal(xs));
        self.compile_expr(&args[1], false);
        let f = self.alloc_local("__lf_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__lf_out");
        self.emit(Op::StoreLocal(out));

        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__lf_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // x := xs[i]
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        let x = self.alloc_local("__lf_x");
        self.emit(Op::StoreLocal(x));

        // if pred(x) { out := out ++ [x] }
        let nid = self.pool.node_id("n_list_filter");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        let skip_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] {
            *off = skip_target - (j_skip as i32 + 1);
        }

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `list.fold(xs, init, f)` — left fold with two-arg combiner.
    fn emit_list_fold(&mut self, args: &[a::CExpr]) {
        // args: xs, init, f
        self.compile_expr(&args[0], false);
        let xs = self.alloc_local("__lo_xs");
        self.emit(Op::StoreLocal(xs));
        self.compile_expr(&args[1], false);
        let acc = self.alloc_local("__lo_acc");
        self.emit(Op::StoreLocal(acc));
        self.compile_expr(&args[2], false);
        let f = self.alloc_local("__lo_f");
        self.emit(Op::StoreLocal(f));

        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__lo_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // acc := f(acc, xs[i])
        let nid = self.pool.node_id("n_list_fold");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(acc));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 2, node_id_idx: nid });
        self.emit(Op::StoreLocal(acc));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(acc));
    }

    // ── Iter[T] operations (#364) ─────────────────────────────────────────
    // Internal representation: `Value::Variant("__IterEager", [list, idx])`
    // for the eager form (a List backing store + Int cursor) and
    // `Value::Variant("__IterLazy", [seed, step_closure])` for the lazy form
    // produced by `iter.unfold` (#376). Both are tagged variants so each op
    // can `TestVariant` at runtime to dispatch. The names start with `__` so
    // they can't be written by user code (uppercase ASCII-letter is required
    // for constructor names, and the underscores keep them out of the
    // user-namespace by convention).

    /// `iter.from_list(xs)` — wrap a list in an eager iterator at position 0.
    fn emit_iter_from_list(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: v, arity: 2 });
    }

    /// `iter.next(it)` — advance one step; returns `Option[(T, Iter[T])]`.
    ///
    /// Dispatches on the iter's variant tag:
    /// - `__IterLazy(seed, step)` (#376) → invoke `step(seed)`. On
    ///   `Some((t, s'))` wrap as `Some((t, __IterLazy(s', step)))`; on
    ///   `None` propagate `None`. The seed advances forward each call.
    /// - `__IterCursor(handle)` (#379) → effect-call `sql.cursor_next(handle)`
    ///   which returns `Option[T]`. On `Some(row)` wrap as
    ///   `Some((row, __IterCursor(handle)))`; on `None` propagate. Handle
    ///   stays stable across calls — state is server-side / mpsc-buffered.
    /// - `__IterEager(list, idx)` → existing positional cursor.
    fn emit_iter_next(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__in_it");
        self.emit(Op::StoreLocal(it));

        // Dispatch: TestVariant pops; we Dup to keep the iter around.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::Dup);
        let lazy_name = self.pool.variant("__IterLazy");
        self.emit(Op::TestVariant(lazy_name));
        let j_to_check_cursor = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // ── lazy path ────────────────────────────────────────────────
        // The Dup'd iter is on stack but we've consumed it via TestVariant,
        // so reload from the local.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0)); // seed
        let seed = self.alloc_local("__in_seed");
        self.emit(Op::StoreLocal(seed));

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(1)); // step closure
        let step = self.alloc_local("__in_step");
        self.emit(Op::StoreLocal(step));

        // Call step(seed) → Option[(T, S)].
        let nid_lazy = self.pool.node_id("n_iter_next_lazy");
        self.emit(Op::LoadLocal(step));
        self.emit(Op::LoadLocal(seed));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid_lazy });
        let opt = self.alloc_local("__in_opt");
        self.emit(Op::StoreLocal(opt));

        // If `step` returned None, propagate it directly.
        self.emit(Op::LoadLocal(opt));
        let some_name = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_name));
        let j_lazy_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((t, new_seed)) — extract the inner tuple, repackage as
        // Some((t, __IterLazy(new_seed, step))) so the next call advances.
        self.emit(Op::LoadLocal(opt));
        self.emit(Op::GetVariantArg(0));     // (t, new_seed)
        let pair = self.alloc_local("__in_pair");
        self.emit(Op::StoreLocal(pair));

        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(0));           // t
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(1));           // new_seed
        self.emit(Op::LoadLocal(step));      // step closure
        let lazy_v = self.pool.variant("__IterLazy");
        self.emit(Op::MakeVariant { name_idx: lazy_v, arity: 2 }); // __IterLazy(new_seed, step)
        self.emit(Op::MakeTuple(2));         // (t, new_iter)
        let some_v = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_v, arity: 1 });
        let j_after_lazy = self.code.len();
        self.emit(Op::Jump(0));

        // Lazy → None: just forward the None.
        let none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_lazy_none] {
            *off = none_t - (j_lazy_none as i32 + 1);
        }
        let none_v = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_v, arity: 0 });
        let j_after_lazy_none = self.code.len();
        self.emit(Op::Jump(0));

        // ── cursor path (#379) ───────────────────────────────────────
        let cursor_check_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_check_cursor] {
            *off = cursor_check_t - (j_to_check_cursor as i32 + 1);
        }

        self.emit(Op::LoadLocal(it));
        self.emit(Op::Dup);
        let cursor_name = self.pool.variant("__IterCursor");
        self.emit(Op::TestVariant(cursor_name));
        let j_to_eager = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Cursor path: extract handle, effect-call sql.cursor_next(handle).
        // The handler returns Option[T] directly. We then wrap as
        // Some((T, __IterCursor(handle))) or forward None.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0));     // handle
        let handle = self.alloc_local("__in_handle");
        self.emit(Op::StoreLocal(handle));

        let kind_idx = self.pool.str("sql");
        let op_idx = self.pool.str("cursor_next");
        let nid_cursor = self.pool.node_id("n_iter_next_cursor");
        self.emit(Op::LoadLocal(handle));
        self.emit(Op::EffectCall {
            kind_idx,
            op_idx,
            arity: 1,
            node_id_idx: nid_cursor,
        });
        let cur_opt = self.alloc_local("__in_cur_opt");
        self.emit(Op::StoreLocal(cur_opt));

        self.emit(Op::LoadLocal(cur_opt));
        let some_c = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_c));
        let j_cursor_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some(row): build Some((row, __IterCursor(handle)))
        self.emit(Op::LoadLocal(cur_opt));
        self.emit(Op::GetVariantArg(0));     // row
        self.emit(Op::LoadLocal(handle));
        let cursor_v = self.pool.variant("__IterCursor");
        self.emit(Op::MakeVariant { name_idx: cursor_v, arity: 1 });
        self.emit(Op::MakeTuple(2));         // (row, __IterCursor(handle))
        let some_c2 = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_c2, arity: 1 });
        let j_after_cursor = self.code.len();
        self.emit(Op::Jump(0));

        // Cursor → None
        let cursor_none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_cursor_none] {
            *off = cursor_none_t - (j_cursor_none as i32 + 1);
        }
        let none_c = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_c, arity: 0 });
        let j_after_cursor_none = self.code.len();
        self.emit(Op::Jump(0));

        // ── eager path ───────────────────────────────────────────────
        let eager_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_eager] {
            *off = eager_t - (j_to_eager as i32 + 1);
        }

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__in_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(1));
        let idx = self.alloc_local("__in_idx");
        self.emit(Op::StoreLocal(idx));

        // if idx < len(list)
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_eager_else = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((item, __IterEager(list, idx+1)))
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::GetListElemDyn);

        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(idx));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
        self.emit(Op::MakeTuple(2));
        let some_e = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_e, arity: 1 });
        let j_after_eager = self.code.len();
        self.emit(Op::Jump(0));

        // Eager → None
        let eager_none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_eager_else] {
            *off = eager_none_t - (j_eager_else as i32 + 1);
        }
        let none_e = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_e, arity: 0 });

        // Converge all paths.
        let end = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_after_lazy] {
            *off = end - (j_after_lazy as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_lazy_none] {
            *off = end - (j_after_lazy_none as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_cursor] {
            *off = end - (j_after_cursor as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_cursor_none] {
            *off = end - (j_after_cursor_none as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_eager] {
            *off = end - (j_after_eager as i32 + 1);
        }
    }

    /// `iter.unfold(seed, step)` — lazy iterator that calls `step(seed)` on
    /// each `iter.next` and threads the new seed forward. Internal value
    /// shape: `__IterLazy(seed, step)`. Step has type `(S) -> Option[(T, S)]`;
    /// returning `None` ends the iteration (#376).
    fn emit_iter_unfold(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false); // seed
        self.compile_expr(&args[1], false); // step
        let lazy = self.pool.variant("__IterLazy");
        self.emit(Op::MakeVariant { name_idx: lazy, arity: 2 });
    }

    /// `iter.is_empty(it)` — true iff no further element. v1 supports the
    /// eager form O(1); on a lazy iter the seed sits in slot 0 and is not a
    /// List, so the VM trips on `GetListLen` rather than returning a wrong
    /// answer. Callers needing lazy support should materialize with
    /// `iter.to_list` first or call `iter.next` and pattern-match.
    fn emit_iter_is_empty(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__ie_it");
        self.emit(Op::StoreLocal(it));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1)); // idx
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0)); // list
        self.emit(Op::GetListLen);                                     // len
        self.emit(Op::NumLt);                                          // idx < len
        self.emit(Op::BoolNot);                                        // NOT(idx < len)
    }

    /// `iter.count(it)` — number of remaining elements (v1: eager-only).
    fn emit_iter_count(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__ic_it");
        self.emit(Op::StoreLocal(it));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        self.emit(Op::GetListLen);                                     // len
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1)); // idx
        self.emit(Op::NumSub);                                         // len - idx
    }

    /// `iter.take(it, n)` — collect up to n elements, return as new Iter.
    fn emit_iter_take(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__itk_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let n    = self.alloc_local("__itk_n");
        self.emit(Op::StoreLocal(n));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__itk_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__itk_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__itk_out");
        self.emit(Op::StoreLocal(out));

        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let cnt  = self.alloc_local("__itk_cnt");
        self.emit(Op::StoreLocal(cnt));

        let loop_top = self.code.len();

        // while cnt < n
        self.emit(Op::LoadLocal(cnt));
        self.emit(Op::LoadLocal(n));
        self.emit(Op::NumLt);
        let j_exit_n = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // AND i < len(list)
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit_l = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // out = out ++ [list[i]]
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        let one = self.pool.int(1);
        // i = i + 1
        self.emit(Op::LoadLocal(i));
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));
        // cnt = cnt + 1
        self.emit(Op::LoadLocal(cnt));
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(cnt));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit_n] { *off = exit_t - (j_exit_n as i32 + 1); }
        if let Op::JumpIfNot(off) = &mut self.code[j_exit_l] { *off = exit_t - (j_exit_l as i32 + 1); }

        // return new __IterEager(out, 0)
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.skip(it, n)` — advance cursor by n (or to end), return new Iter.
    fn emit_iter_skip(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__isk_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let n    = self.alloc_local("__isk_n");
        self.emit(Op::StoreLocal(n));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__isk_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let idx  = self.alloc_local("__isk_idx");
        self.emit(Op::StoreLocal(idx));

        // raw = idx + n
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::LoadLocal(n));
        self.emit(Op::NumAdd);
        let raw  = self.alloc_local("__isk_raw");
        self.emit(Op::StoreLocal(raw));

        // new_idx = if raw < len then raw else len
        self.emit(Op::LoadLocal(raw));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_use_raw = self.code.len();
        self.emit(Op::JumpIf(0));

        // use len
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // use raw
        let raw_t = self.code.len() as i32;
        if let Op::JumpIf(off) = &mut self.code[j_use_raw] { *off = raw_t - (j_use_raw as i32 + 1); }
        self.emit(Op::LoadLocal(raw));

        let end_t = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] { *off = end_t - (j_end as i32 + 1); }

        // new_idx on stack; build new __IterEager(list, new_idx)
        let new_idx = self.alloc_local("__isk_ni");
        self.emit(Op::StoreLocal(new_idx));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(new_idx));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.to_list(it)` — materialise remaining elements into a List.
    ///
    /// Dispatches on the iter variant (#376):
    /// - `__IterLazy`: repeatedly call `step(seed)`; on `Some((t, s'))` append
    ///   `t` and continue with `s'`; on `None` stop. May hang on truly
    ///   infinite producers — that's documented as a v1 limitation, the
    ///   step-limit-protected caller is what catches misuse.
    /// - `__IterEager`: slice the backing list from `idx` onward (O(n) walk).
    fn emit_iter_to_list(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__itl_it");
        self.emit(Op::StoreLocal(it));

        // Build the output list up-front, shared across both paths.
        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__itl_out");
        self.emit(Op::StoreLocal(out));

        // Dispatch on variant tag.
        self.emit(Op::LoadLocal(it));
        let lazy_name = self.pool.variant("__IterLazy");
        self.emit(Op::TestVariant(lazy_name));
        let j_to_eager = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // ── lazy path ─────────────────────────────────────────────────
        // seed and step closure live in locals; we update seed each iteration.
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let seed = self.alloc_local("__itl_seed");
        self.emit(Op::StoreLocal(seed));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let step = self.alloc_local("__itl_step");
        self.emit(Op::StoreLocal(step));

        let lazy_loop = self.code.len();
        let nid_lazy = self.pool.node_id("n_iter_to_list_lazy");
        self.emit(Op::LoadLocal(step));
        self.emit(Op::LoadLocal(seed));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid_lazy });
        let opt = self.alloc_local("__itl_opt");
        self.emit(Op::StoreLocal(opt));

        // If None, drop out of the lazy loop.
        self.emit(Op::LoadLocal(opt));
        let some_name = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_name));
        let j_lazy_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((t, new_seed)): append t to out, replace seed.
        self.emit(Op::LoadLocal(opt));
        self.emit(Op::GetVariantArg(0));
        let pair = self.alloc_local("__itl_pair");
        self.emit(Op::StoreLocal(pair));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(pair)); self.emit(Op::GetElem(0));
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(pair)); self.emit(Op::GetElem(1));
        self.emit(Op::StoreLocal(seed));

        let jback_lazy = self.code.len();
        self.emit(Op::Jump((lazy_loop as i32) - (jback_lazy as i32 + 1)));

        let lazy_exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_lazy_exit] {
            *off = lazy_exit_t - (j_lazy_exit as i32 + 1);
        }
        let j_after_lazy = self.code.len();
        self.emit(Op::Jump(0));

        // ── eager path ────────────────────────────────────────────────
        let eager_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_eager] {
            *off = eager_t - (j_to_eager as i32 + 1);
        }

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__itl_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i = self.alloc_local("__itl_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_t - (j_exit as i32 + 1);
        }

        // Converge: lazy path falls through here too.
        let converge = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_after_lazy] {
            *off = converge - (j_after_lazy as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `iter.map(it, f)` — apply `f` to each remaining element; returns new Iter.
    fn emit_iter_map(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__im_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let f    = self.alloc_local("__im_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__im_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__im_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__im_out");
        self.emit(Op::StoreLocal(out));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        let nid = self.pool.node_id("n_iter_map");
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }

        let zero = self.pool.int(0);
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.filter(it, pred)` — keep elements where pred is true; returns new Iter.
    fn emit_iter_filter(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__if_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let f    = self.alloc_local("__if_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__if_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__if_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__if_out");
        self.emit(Op::StoreLocal(out));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // elem := list[i]
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        let x    = self.alloc_local("__if_x");
        self.emit(Op::StoreLocal(x));

        let nid = self.pool.node_id("n_iter_filter");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        let skip_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] { *off = skip_t - (j_skip as i32 + 1); }

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }

        let zero = self.pool.int(0);
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.fold(it, init, f)` — left fold over remaining elements.
    fn emit_iter_fold(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__ifo_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let acc  = self.alloc_local("__ifo_acc");
        self.emit(Op::StoreLocal(acc));

        self.compile_expr(&args[2], false);
        let f    = self.alloc_local("__ifo_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__ifo_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__ifo_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        let nid = self.pool.node_id("n_iter_fold");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(acc));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 2, node_id_idx: nid });
        self.emit(Op::StoreLocal(acc));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }
        self.emit(Op::LoadLocal(acc));
    }

    /// `map.fold(m, init, f)` — left fold over `Map[K, V]` entries with a
    /// three-arg combiner `f(acc, k, v)`. Iteration order matches
    /// `map.entries` (BTreeMap-sorted by key). Materializes the entry
    /// list once via the runtime's `("map", "entries")` op, then runs
    /// the same inline loop as `list.fold`.
    fn emit_map_fold(&mut self, args: &[a::CExpr], node_id_idx: u32) {
        // xs := map.entries(m)
        self.compile_expr(&args[0], false);
        let map_kind = self.pool.str("map");
        let entries_op = self.pool.str("entries");
        self.emit(Op::EffectCall {
            kind_idx: map_kind,
            op_idx: entries_op,
            arity: 1,
            node_id_idx,
        });
        let xs = self.alloc_local("__mf_xs");
        self.emit(Op::StoreLocal(xs));

        // acc := init
        self.compile_expr(&args[1], false);
        let acc = self.alloc_local("__mf_acc");
        self.emit(Op::StoreLocal(acc));

        // f := <closure>
        self.compile_expr(&args[2], false);
        let f = self.alloc_local("__mf_f");
        self.emit(Op::StoreLocal(f));

        // i := 0
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__mf_i");
        self.emit(Op::StoreLocal(i));

        // loop_top: while i < len(xs)
        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // pair := xs[i]
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        let pair = self.alloc_local("__mf_pair");
        self.emit(Op::StoreLocal(pair));

        // acc := f(acc, pair.0, pair.1)
        let nid = self.pool.node_id("n_map_fold");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(acc));
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(0));
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(1));
        self.emit(Op::CallClosure { arity: 3, node_id_idx: nid });
        self.emit(Op::StoreLocal(acc));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(acc));
    }

    /// Inline pattern: `<module>.map(v, f)` and friends.
    /// `wrap_with`: variant tag whose payload triggers the call (Ok / Some / Err).
    /// `wrap_result`: if true, wrap the closure's result back in `wrap_with`
    /// (map shape); if false, expect the closure to return a wrapped value
    /// itself (and_then shape).
    fn emit_variant_map(
        &mut self,
        args: &[a::CExpr],
        wrap_with: &str,
        wrap_result: bool,
    ) {
        // args[0] = the wrapped value (Result/Option), args[1] = closure
        let wrap_idx = self.pool.variant(wrap_with);

        // Compile and store the value into a local, evaluate closure on top of stack.
        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__hov");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__hof");
        self.emit(Op::StoreLocal(f_slot));

        // Stack discipline:
        //   load val ⇒ [v]
        //   dup     ⇒ [v, v]
        //   test    ⇒ [v, Bool]
        //   jumpifnot ⇒ [v]
        // Both branches end with [v] before the branch body.
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(wrap_idx));
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Matched arm: extract payload, call closure on it.
        self.emit(Op::GetVariantArg(0));
        let arg_slot = self.alloc_local("__hov_arg");
        self.emit(Op::StoreLocal(arg_slot));
        self.emit(Op::LoadLocal(f_slot));
        self.emit(Op::LoadLocal(arg_slot));
        let nid = self.pool.node_id("n_hov");
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        if wrap_result {
            self.emit(Op::MakeVariant { name_idx: wrap_idx, arity: 1 });
        }
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // Skip arm: stack already has [v] from the failed Dup; nothing to do.
        let skip_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] {
            *off = skip_target - (j_skip as i32 + 1);
        }

        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    /// Sibling of `emit_variant_map` for the recovery combinators
    /// `result.or_else` and `option.or_else`. Differences from
    /// `emit_variant_map`:
    ///   - matches on the *negative* variant (`Err` / `None`)
    ///   - the closure's result becomes the call's result directly,
    ///     with no wrapping (it is itself a `Result` / `Option`)
    ///   - `option.or_else`'s closure takes zero args (`None` has no
    ///     payload to forward)
    fn emit_variant_or_else(
        &mut self,
        args: &[a::CExpr],
        match_on: &str,
        closure_arity: u16,
    ) {
        let match_idx = self.pool.variant(match_on);

        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__hoe");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__hoe_f");
        self.emit(Op::StoreLocal(f_slot));

        // Stack discipline mirrors emit_variant_map:
        //   load val      ⇒ [v]
        //   dup           ⇒ [v, v]
        //   test          ⇒ [v, Bool]
        //   jumpifnot     ⇒ [v]
        // The unmatched arm leaves [v] (Ok/Some unchanged); the
        // matched arm pops [v] and pushes the closure's result.
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(match_idx));
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Matched arm: pop the duplicate left on the stack,
        // then call the closure with whatever payload it expects.
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(f_slot));
        if closure_arity == 1 {
            self.emit(Op::LoadLocal(val_slot));
            self.emit(Op::GetVariantArg(0));
        }
        let nid = self.pool.node_id("n_hoe");
        self.emit(Op::CallClosure { arity: closure_arity, node_id_idx: nid });

        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // Unmatched arm: stack already holds [v]; nothing to do.
        let skip_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] {
            *off = skip_target - (j_skip as i32 + 1);
        }

        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    /// `option.unwrap_or_else(opt, f)` — lazy default via zero-arg thunk.
    ///   Some(x) → x          (unwrap; no wrapping)
    ///   None    → f()        (call thunk; return its result directly)
    fn emit_option_unwrap_or_else(&mut self, args: &[a::CExpr]) {
        let some_idx = self.pool.variant("Some");

        // Compile opt and f; stash both so they're accessible on both arms.
        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__uoe_val");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__uoe_f");
        self.emit(Op::StoreLocal(f_slot));

        // Test whether opt is Some.
        //   load val ⇒ [v]
        //   dup      ⇒ [v, v]
        //   test     ⇒ [v, Bool]
        //   jumpifnot → None arm
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(some_idx));
        let j_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some arm: extract the payload from [v] left on the stack.
        self.emit(Op::GetVariantArg(0));
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // None arm: pop the [v] duplicate, call the thunk.
        let none_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_none] {
            *off = none_target - (j_none as i32 + 1);
        }
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(f_slot));
        let nid = self.pool.node_id("n_uoe");
        self.emit(Op::CallClosure { arity: 0, node_id_idx: nid });

        // Patch jump-to-end from Some arm.
        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    // ---- std.flow trampolines ----------------------------------------
    //
    // Each flow.<op>(c1, c2, ...) call site:
    //   1. compiles its closure args and leaves them on the stack
    //   2. registers a fresh "trampoline" Function whose body invokes
    //      those captured closures appropriately
    //   3. emits MakeClosure { fn_id: trampoline, capture_count: N }
    //
    // The trampoline's parameter layout is [capture_0, ..., capture_{N-1},
    // arg_0, ...]: captures first, the closure's own args after.

    /// Allocate a fresh fn_id for a trampoline and install its bytecode.
    /// Trampolines are the one Function-creation path that already has
    /// the body in hand at install time (top-level fns and lambdas have
    /// it filled in later), so we compute `body_hash` immediately. The
    /// final hash pass at the end of `compile_program` is a no-op here.
    fn install_trampoline(&mut self, name: &str, arity: u16, locals_count: u16, code: Vec<Op>) -> u32 {
        let fn_id = self.next_fn_id.len() as u32;
        let body_hash = crate::program::compute_body_hash(arity, locals_count, &code);
        self.next_fn_id.push(Function {
            name: name.into(),
            arity,
            locals_count,
            code,
            effects: Vec::new(),
            body_hash,
            // Trampolines (flow.sequential / parallel / etc.) don't
            // surface refined params at this layer.
            refinements: Vec::new(),
        });
        fn_id
    }

    /// `flow.sequential(f, g)` returns a closure `(x) -> g(f(x))`.
    fn emit_flow_sequential(&mut self, args: &[a::CExpr]) {
        // Push f, g; build the trampoline closure with 2 captures.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_flow_sequential");
        let code = vec![
            // Locals: [f=0, g=1, x=2]
            Op::LoadLocal(0),                                  // push f
            Op::LoadLocal(2),                                  // push x
            Op::CallClosure { arity: 1, node_id_idx: nid },    // r = f(x)
            // stack: [r]
            Op::StoreLocal(3),                                 // tmp = r
            Op::LoadLocal(1),                                  // push g
            Op::LoadLocal(3),                                  // push tmp
            Op::CallClosure { arity: 1, node_id_idx: nid },    // r = g(tmp)
            Op::Return,
        ];
        let fn_id = self.install_trampoline("__flow_sequential", 3, 4, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.parallel(fa, fb)` returns a closure `() -> (fa(), fb())`.
    /// Implementation is sequential: each function is called in order
    /// and the results are packed into a 2-tuple. The spec (§11.2)
    /// allows the runtime to apply true parallelism here; that needs
    /// a thread-safe handler split and is left to a follow-up. The
    /// signature is what users program against — sequential vs threaded
    /// is an implementation detail invisible to the type system.
    fn emit_flow_parallel(&mut self, args: &[a::CExpr]) {
        // Push fa, fb; build a 0-arg trampoline closure with 2 captures.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_flow_parallel");
        let code = vec![
            // Locals: [fa=0, fb=1]
            Op::LoadLocal(0),                                  // push fa
            Op::CallClosure { arity: 0, node_id_idx: nid },    // a = fa()
            Op::LoadLocal(1),                                  // push fb
            Op::CallClosure { arity: 0, node_id_idx: nid },    // b = fb()
            Op::MakeTuple(2),                                  // (a, b)
            Op::Return,
        ];
        let fn_id = self.install_trampoline("__flow_parallel", 2, 2, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.parallel_list(actions)` runs each 0-arg closure in `actions`
    /// and returns the results as a list in input order. Variadic
    /// counterpart to `flow.parallel`. Sequential under the hood — the
    /// spec (§11.2) reserves true threading for a future scheduler.
    /// Compiled inline (mirrors `list.map`) so closure args can flow
    /// through `CallClosure` without a heap-allocated trampoline.
    fn emit_flow_parallel_list(&mut self, args: &[a::CExpr]) {
        // xs := actions
        self.compile_expr(&args[0], false);
        let xs = self.alloc_local("__fpl_xs");
        self.emit(Op::StoreLocal(xs));

        // out := []
        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__fpl_out");
        self.emit(Op::StoreLocal(out));

        // i := 0
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__fpl_i");
        self.emit(Op::StoreLocal(i));

        // loop_top: while i < len(xs) { ... }
        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::NumLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // body: out := out ++ [xs[i]()]
        let nid = self.pool.node_id("n_flow_parallel_list");
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 0, node_id_idx: nid });
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::NumAdd);
        self.emit(Op::StoreLocal(i));

        // jump back
        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        // exit: patch j_exit, push out
        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `flow.branch(cond, t, f)` returns a closure `(x) -> if cond(x) then t(x) else f(x)`.
    fn emit_flow_branch(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        self.compile_expr(&args[2], false);
        let nid = self.pool.node_id("n_flow_branch");
        let mut code = vec![
            // Locals: [cond=0, t=1, f=2, x=3]
            Op::LoadLocal(0),                               // push cond
            Op::LoadLocal(3),                               // push x
            Op::CallClosure { arity: 1, node_id_idx: nid }, // bool
        ];
        let j_false = code.len();
        code.push(Op::JumpIfNot(0));                        // patched
        // true arm: t(x)
        code.push(Op::LoadLocal(1));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: nid });
        code.push(Op::Return);
        // false arm
        let false_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_false] {
            *off = false_target - (j_false as i32 + 1);
        }
        code.push(Op::LoadLocal(2));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: nid });
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_branch", 4, 4, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 3 });
    }

    /// `flow.retry(f, max_attempts)` returns a closure `(x) -> Result[U, E]`
    /// that calls `f(x)` up to `max_attempts` times, returning the first
    /// `Ok` or the final `Err`.
    fn emit_flow_retry(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let call_nid = self.pool.node_id("n_flow_retry");
        let ok_idx = self.pool.variant("Ok");
        let zero_const = self.pool.int(0);
        let one_const = self.pool.int(1);
        // Locals: [f=0, max=1, x=2, i=3, last=4]
        let mut code = vec![
            // i := 0
            Op::PushConst(zero_const),
            Op::StoreLocal(3),
        ];
        // loop_top: while i < max
        let loop_top = code.len() as i32;
        code.push(Op::LoadLocal(3));
        code.push(Op::LoadLocal(1));
        code.push(Op::NumLt);
        let j_done = code.len();
        code.push(Op::JumpIfNot(0));                       // patched

        // body: r := f(x); last := r
        code.push(Op::LoadLocal(0));
        code.push(Op::LoadLocal(2));
        code.push(Op::CallClosure { arity: 1, node_id_idx: call_nid });
        code.push(Op::StoreLocal(4));

        // Test variant Ok on last; if so, return last.
        code.push(Op::LoadLocal(4));
        code.push(Op::TestVariant(ok_idx));
        let j_was_err = code.len();
        code.push(Op::JumpIfNot(0));                       // patched: skip return
        code.push(Op::LoadLocal(4));
        code.push(Op::Return);

        // was_err: i := i + 1; jump loop_top
        let was_err_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_was_err] {
            *off = was_err_target - (j_was_err as i32 + 1);
        }
        code.push(Op::LoadLocal(3));
        code.push(Op::PushConst(one_const));
        code.push(Op::NumAdd);
        code.push(Op::StoreLocal(3));
        let pc_after_jump = code.len() as i32 + 1;
        code.push(Op::Jump(loop_top - pc_after_jump));

        // done: return last (the final Err, or Unit if max=0).
        let done_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_done] {
            *off = done_target - (j_done as i32 + 1);
        }
        code.push(Op::LoadLocal(4));
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_retry", 3, 5, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.retry_with_backoff(f, attempts, base_ms)` (#226). Variant
    /// of `flow.retry` that sleeps between attempts. The first
    /// attempt fires immediately; attempt k > 1 waits `base_ms *
    /// 2^(k-2)` ms before retrying. Sleeps go through
    /// `time.sleep_ms`, which is why the resulting closure carries
    /// `[time]` in its effect row even though the underlying `f` is
    /// pure.
    fn emit_flow_retry_with_backoff(&mut self, args: &[a::CExpr]) {
        // Push captures: f, max, base_ms. The trampoline takes one
        // call-time arg `x`, so capture_count = 3, arity = 4.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        self.compile_expr(&args[2], false);
        let call_nid    = self.pool.node_id("n_flow_retry_backoff");
        let sleep_nid   = self.pool.node_id("n_flow_retry_backoff_sleep");
        let kind_idx    = self.pool.str("time");
        let op_idx      = self.pool.str("sleep_ms");
        let ok_idx      = self.pool.variant("Ok");
        let zero_const  = self.pool.int(0);
        let one_const   = self.pool.int(1);
        let two_const   = self.pool.int(2);
        // Locals layout:
        //   0=f, 1=max, 2=base_ms (captures)
        //   3=x (arg)
        //   4=i, 5=last, 6=next_delay (working state)
        let mut code = vec![
            // next_delay := base_ms
            Op::LoadLocal(2),
            Op::StoreLocal(6),
            // i := 0
            Op::PushConst(zero_const),
            Op::StoreLocal(4),
        ];

        let loop_top = code.len() as i32;
        // while i < max
        code.push(Op::LoadLocal(4));
        code.push(Op::LoadLocal(1));
        code.push(Op::NumLt);
        let j_done = code.len();
        code.push(Op::JumpIfNot(0)); // patched

        // if i > 0: time.sleep_ms(next_delay); next_delay := next_delay * 2
        code.push(Op::PushConst(zero_const));
        code.push(Op::LoadLocal(4));
        code.push(Op::NumLt);                // 0 < i ?
        let j_no_sleep = code.len();
        code.push(Op::JumpIfNot(0));         // patched: skip the sleep block
        // Sleep
        code.push(Op::LoadLocal(6));         // arg = next_delay
        code.push(Op::EffectCall {
            kind_idx, op_idx, arity: 1, node_id_idx: sleep_nid,
        });
        code.push(Op::Pop);                  // discard the Unit result
        // next_delay := next_delay * 2
        code.push(Op::LoadLocal(6));
        code.push(Op::PushConst(two_const));
        code.push(Op::NumMul);
        code.push(Op::StoreLocal(6));
        // patch the no-sleep skip
        let after_sleep = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_no_sleep] {
            *off = after_sleep - (j_no_sleep as i32 + 1);
        }

        // last := f(x)
        code.push(Op::LoadLocal(0));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: call_nid });
        code.push(Op::StoreLocal(5));

        // if Ok(last): return last
        code.push(Op::LoadLocal(5));
        code.push(Op::TestVariant(ok_idx));
        let j_was_err = code.len();
        code.push(Op::JumpIfNot(0)); // patched
        code.push(Op::LoadLocal(5));
        code.push(Op::Return);

        // was_err: i := i + 1; jump loop_top
        let was_err_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_was_err] {
            *off = was_err_target - (j_was_err as i32 + 1);
        }
        code.push(Op::LoadLocal(4));
        code.push(Op::PushConst(one_const));
        code.push(Op::NumAdd);
        code.push(Op::StoreLocal(4));
        let pc_after_jump = code.len() as i32 + 1;
        code.push(Op::Jump(loop_top - pc_after_jump));

        // done: return last (the final Err, or Unit if max=0).
        let done_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_done] {
            *off = done_target - (j_done as i32 + 1);
        }
        code.push(Op::LoadLocal(5));
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_retry_backoff", 4, 7, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 3 });
    }
}

/// Collect free variables referenced in `e` that are not in `bound`.
/// Mutates `bound` to track let/lambda introductions during the walk;
/// the caller's set is preserved on return because Rust's borrow rules
/// force us to clone for sub-scopes that rebind a name.
fn free_vars(e: &a::CExpr, bound: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
    match e {
        a::CExpr::Literal { .. } => {}
        a::CExpr::Var { name } => {
            if !bound.contains(name) && !out.contains(name) {
                out.push(name.clone());
            }
        }
        a::CExpr::Call { callee, args } => {
            free_vars(callee, bound, out);
            for a in args { free_vars(a, bound, out); }
        }
        a::CExpr::Let { name, value, body, .. } => {
            free_vars(value, bound, out);
            let was_bound = bound.contains(name);
            bound.insert(name.clone());
            free_vars(body, bound, out);
            if !was_bound { bound.remove(name); }
        }
        a::CExpr::Match { scrutinee, arms } => {
            free_vars(scrutinee, bound, out);
            for arm in arms {
                let mut local_bound = bound.clone();
                pattern_binders(&arm.pattern, &mut local_bound);
                free_vars(&arm.body, &mut local_bound, out);
            }
        }
        a::CExpr::Block { statements, result } => {
            let mut local_bound = bound.clone();
            for s in statements { free_vars(s, &mut local_bound, out); }
            free_vars(result, &mut local_bound, out);
        }
        a::CExpr::Constructor { args, .. } => {
            for a in args { free_vars(a, bound, out); }
        }
        a::CExpr::RecordLit { fields } => {
            for f in fields { free_vars(&f.value, bound, out); }
        }
        a::CExpr::TupleLit { items } | a::CExpr::ListLit { items } => {
            for it in items { free_vars(it, bound, out); }
        }
        a::CExpr::FieldAccess { value, .. } => free_vars(value, bound, out),
        a::CExpr::Lambda { params, body, .. } => {
            let mut inner = bound.clone();
            for p in params { inner.insert(p.name.clone()); }
            free_vars(body, &mut inner, out);
        }
        a::CExpr::BinOp { lhs, rhs, .. } => {
            free_vars(lhs, bound, out);
            free_vars(rhs, bound, out);
        }
        a::CExpr::UnaryOp { expr, .. } => free_vars(expr, bound, out),
        a::CExpr::Return { value } => free_vars(value, bound, out),
    }
}

fn pattern_binders(p: &a::Pattern, bound: &mut std::collections::HashSet<String>) {
    match p {
        a::Pattern::PWild | a::Pattern::PLiteral { .. } => {}
        a::Pattern::PVar { name } => { bound.insert(name.clone()); }
        a::Pattern::PConstructor { args, .. } => {
            for a in args { pattern_binders(a, bound); }
        }
        a::Pattern::PRecord { fields } => {
            for f in fields { pattern_binders(&f.pattern, bound); }
        }
        a::Pattern::PTuple { items } => {
            for it in items { pattern_binders(it, bound); }
        }
    }
}
