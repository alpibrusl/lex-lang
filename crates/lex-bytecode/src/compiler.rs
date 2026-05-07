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
                if let Op::JumpIfNot(off) = &mut self.code[j] {
                    *off = fail_target - (j as i32 + 1);
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
                self.emit(Op::Dup);
                self.emit(Op::TestVariant(name_idx));
                let j = self.code.len();
                self.emit(Op::JumpIfNot(0));
                fails.push(j);
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

        // Filter to those that are in the enclosing locals (captures) —
        // skip globals (function names) which are referenced directly.
        let captures: Vec<String> = frees.into_iter()
            .filter(|n| self.locals.contains_key(n) && !self.function_names.contains_key(n))
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
            ("list", "map") => self.emit_list_map(args),
            ("list", "filter") => self.emit_list_filter(args),
            ("list", "fold") => self.emit_list_fold(args),
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
