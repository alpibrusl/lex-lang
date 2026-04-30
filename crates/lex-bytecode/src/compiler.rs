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
            });
        }
    }

    let mut pool = ConstPool::default();
    let function_names = p.function_names.clone();
    let module_aliases = p.module_aliases.clone();

    for s in stages {
        if let a::Stage::FnDecl(fd) = s {
            let mut fc = FnCompiler {
                code: Vec::new(),
                locals: IndexMap::new(),
                next_local: 0,
                peak_local: 0,
                pool: &mut pool,
                function_names: &function_names,
                module_aliases: &module_aliases,
            };
            for param in &fd.params {
                let i = fc.next_local;
                fc.locals.insert(param.name.clone(), i);
                fc.next_local += 1;
                fc.peak_local = fc.next_local;
            }
            fc.compile_expr(&fd.body, true);
            fc.code.push(Op::Return);
            let idx = function_names[&fd.name];
            p.functions[idx as usize].code = fc.code;
            p.functions[idx as usize].locals_count = fc.peak_local;
        }
    }
    p.constants = pool.pool;
    p
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
                let i = *self.locals.get(name).unwrap_or_else(|| panic!("unknown local: {name}"));
                self.emit(Op::LoadLocal(i));
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
            a::CExpr::Call { callee, args } => self.compile_call(callee, args, tail),
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
            a::CExpr::Lambda { .. } => panic!("lambda not supported in M4"),
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

    fn compile_call(&mut self, callee: &a::CExpr, args: &[a::CExpr], tail: bool) {
        // Module function call: `alias.op(args)` where `alias` is an imported
        // module ⇒ EffectCall(kind=module_name, op=field_name).
        if let a::CExpr::FieldAccess { value, field } = callee {
            if let a::CExpr::Var { name } = value.as_ref() {
                if let Some(module) = self.module_aliases.get(name) {
                    for a in args { self.compile_expr(a, false); }
                    let kind_idx = self.pool.str(module);
                    let op_idx = self.pool.str(field);
                    self.emit(Op::EffectCall {
                        kind_idx,
                        op_idx,
                        arity: args.len() as u16,
                    });
                    let _ = tail; // EffectCall doesn't tail-optimize.
                    return;
                }
            }
        }
        match callee {
            a::CExpr::Var { name } if self.function_names.contains_key(name) => {
                for a in args { self.compile_expr(a, false); }
                let fn_id = self.function_names[name];
                if tail {
                    self.emit(Op::TailCall { fn_id, arity: args.len() as u16 });
                } else {
                    self.emit(Op::Call { fn_id, arity: args.len() as u16 });
                }
            }
            other => panic!("unsupported callee: {other:?}"),
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
}
