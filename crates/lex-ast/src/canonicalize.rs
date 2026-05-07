//! Canonicalizer: `SyntaxTree` → `CanonicalAst` per spec §5.3.
//!
//! Rules:
//! 1. Sort record-literal fields and record-type fields alphabetically.
//! 2. Sort union variants alphabetically.
//! 3. `if c { a } else { b }` → `Match(c, [Arm(true, a), Arm(false, b)])`.
//! 4. `e?` → the canonical Match expansion from §3.10.
//! 5. Identifiers starting with an uppercase letter are constructors
//!    (in expressions and in patterns). Lowercase idents are vars/binders.
//! 6. `Pipe(x, f)` → `Call(f, [x])`; `Pipe(x, Call(f, args))` → `Call(f, [x, args...])`.
//! 7. Top-level: each `Item` becomes one `Stage`. Order is preserved (no
//!    dependency reordering yet — TODO §5.3 rule 3).

use crate::canonical::*;
use lex_syntax::syntax as s;

pub fn canonicalize_program(program: &s::Program) -> Vec<Stage> {
    let raw: Vec<Stage> = program.items.iter().map(canonicalize_item).collect();
    // Dead-branch elimination (#228): runs *here*, before type-check
    // and bytecode emission, so the inferred effect set reflects only
    // live branches. `if true { ... } else { ... }` is desugared into
    // a `Match` over a Bool literal upstream; the pass folds that to
    // the matching arm's body in a single depth-first walk.
    crate::dead_branch::eliminate_dead_branches_in_stages(raw)
}

pub fn canonicalize_item(item: &s::Item) -> Stage {
    match item {
        s::Item::Import(i) => Stage::Import(Import {
            reference: i.reference.clone(),
            alias: i.alias.clone(),
        }),
        s::Item::TypeDecl(td) => Stage::TypeDecl(canonicalize_type_decl(td)),
        s::Item::FnDecl(fd) => Stage::FnDecl(canonicalize_fn_decl(fd)),
    }
}

fn canonicalize_type_decl(td: &s::TypeDecl) -> TypeDecl {
    TypeDecl {
        name: td.name.clone(),
        params: td.params.clone(),
        definition: canonicalize_type(&td.definition),
    }
}

fn canonicalize_fn_decl(fd: &s::FnDecl) -> FnDecl {
    FnDecl {
        name: fd.name.clone(),
        type_params: fd.type_params.clone(),
        params: fd
            .params
            .iter()
            .map(|p| Param { name: p.name.clone(), ty: canonicalize_type(&p.ty) })
            .collect(),
        effects: fd.effects.iter().map(canonicalize_effect).collect(),
        return_type: canonicalize_type(&fd.return_type),
        body: canonicalize_block(&fd.body),
    }
}

fn canonicalize_effect(e: &s::Effect) -> Effect {
    Effect {
        name: e.name.clone(),
        arg: e.arg.as_ref().map(|a| match a {
            s::EffectArg::Str(s) => EffectArg::Str { value: s.clone() },
            s::EffectArg::Int(n) => EffectArg::Int { value: *n },
            s::EffectArg::Ident(s) => EffectArg::Ident { value: s.clone() },
        }),
    }
}

fn canonicalize_type(t: &s::TypeExpr) -> TypeExpr {
    match t {
        s::TypeExpr::Named { name, args } => TypeExpr::Named {
            name: name.clone(),
            args: args.iter().map(canonicalize_type).collect(),
        },
        s::TypeExpr::Record(fs) => {
            let mut fields: Vec<TypeField> = fs
                .iter()
                .map(|f| TypeField { name: f.name.clone(), ty: canonicalize_type(&f.ty) })
                .collect();
            fields.sort_by(|a, b| a.name.cmp(&b.name));
            TypeExpr::Record { fields }
        }
        s::TypeExpr::Tuple(items) => TypeExpr::Tuple {
            items: items.iter().map(canonicalize_type).collect(),
        },
        s::TypeExpr::Function { params, effects, ret } => TypeExpr::Function {
            params: params.iter().map(canonicalize_type).collect(),
            effects: {
                let mut es: Vec<Effect> = effects.iter().map(canonicalize_effect).collect();
                es.sort_by(|a, b| a.name.cmp(&b.name));
                es
            },
            ret: Box::new(canonicalize_type(ret)),
        },
        s::TypeExpr::Union(variants) => {
            let mut vs: Vec<UnionVariant> = variants
                .iter()
                .map(|v| UnionVariant {
                    name: v.name.clone(),
                    payload: v.payload.as_ref().map(canonicalize_type),
                })
                .collect();
            vs.sort_by(|a, b| a.name.cmp(&b.name));
            TypeExpr::Union { variants: vs }
        }
        s::TypeExpr::Refined { base, binding, predicate } => TypeExpr::Refined {
            base: Box::new(canonicalize_type(base)),
            binding: binding.clone(),
            predicate: Box::new(canonicalize_expr(predicate)),
        },
    }
}

fn canonicalize_block(b: &s::Block) -> CExpr {
    // Convert `{ stmt1; stmt2; ...; result }` into nested Lets where possible
    // so binders carry through to body. For pure expressions in statement
    // position we keep them in `Block.statements` (this is a CExpr::Block).
    let result = canonicalize_expr(&b.result);
    fold_block(&b.statements[..], result)
}

/// Fold statements into nested `Let { body: ... }` chains. Bare `Statement::Expr`
/// becomes a `Block { statements: [...], result: ... }` — but only if there
/// are non-let statements in the chain. For all-let chains we collapse to a
/// chain of `Let { body }` so type-checking and evaluation are linear.
fn fold_block(stmts: &[s::Statement], result: CExpr) -> CExpr {
    if stmts.is_empty() {
        return result;
    }
    // Walk from the end, building nested Lets / blocks.
    let mut acc = result;
    let mut pending_exprs: Vec<CExpr> = Vec::new();
    for stmt in stmts.iter().rev() {
        match stmt {
            s::Statement::Let { name, ty, value } => {
                if !pending_exprs.is_empty() {
                    pending_exprs.reverse();
                    let body = std::mem::replace(&mut acc, CExpr::Literal { value: CLit::Unit });
                    acc = CExpr::Block { statements: pending_exprs.clone(), result: Box::new(body) };
                    pending_exprs.clear();
                }
                acc = CExpr::Let {
                    name: name.clone(),
                    ty: ty.as_ref().map(canonicalize_type),
                    value: Box::new(canonicalize_expr(value)),
                    body: Box::new(acc),
                };
            }
            s::Statement::Expr(e) => {
                pending_exprs.push(canonicalize_expr(e));
            }
        }
    }
    if !pending_exprs.is_empty() {
        pending_exprs.reverse();
        acc = CExpr::Block { statements: pending_exprs, result: Box::new(acc) };
    }
    acc
}

fn canonicalize_expr(e: &s::Expr) -> CExpr {
    match e {
        s::Expr::Lit(l) => CExpr::Literal { value: canonicalize_lit(l) },
        s::Expr::Var(name) => {
            if is_constructor_name(name) {
                CExpr::Constructor { name: name.clone(), args: Vec::new() }
            } else {
                CExpr::Var { name: name.clone() }
            }
        }
        s::Expr::Block(b) => canonicalize_block(b),
        s::Expr::Call { callee, args } => {
            // Special case: a Call whose callee is an uppercase-named Var is a constructor.
            if let s::Expr::Var(name) = callee.as_ref() {
                if is_constructor_name(name) {
                    return CExpr::Constructor {
                        name: name.clone(),
                        args: args.iter().map(canonicalize_expr).collect(),
                    };
                }
            }
            CExpr::Call {
                callee: Box::new(canonicalize_expr(callee)),
                args: args.iter().map(canonicalize_expr).collect(),
            }
        }
        s::Expr::Pipe { left, right } => {
            let lc = canonicalize_expr(left);
            // x |> f ≡ Call(f, [x]); x |> f(args) ≡ Call(f, [x, args...]).
            match canonicalize_expr(right) {
                CExpr::Call { callee, args } => {
                    let mut new_args = Vec::with_capacity(args.len() + 1);
                    new_args.push(lc);
                    new_args.extend(args);
                    CExpr::Call { callee, args: new_args }
                }
                CExpr::Constructor { name, args } => {
                    let mut new_args = Vec::with_capacity(args.len() + 1);
                    new_args.push(lc);
                    new_args.extend(args);
                    CExpr::Constructor { name, args: new_args }
                }
                other => CExpr::Call { callee: Box::new(other), args: vec![lc] },
            }
        }
        s::Expr::Try(inner) => {
            // §3.10 desugar
            let v = canonicalize_expr(inner);
            CExpr::Match {
                scrutinee: Box::new(v),
                arms: vec![
                    Arm {
                        pattern: Pattern::PConstructor {
                            name: "Ok".into(),
                            args: vec![Pattern::PVar { name: "v".into() }],
                        },
                        body: CExpr::Var { name: "v".into() },
                    },
                    Arm {
                        pattern: Pattern::PConstructor {
                            name: "Err".into(),
                            args: vec![Pattern::PVar { name: "e".into() }],
                        },
                        body: CExpr::Return {
                            value: Box::new(CExpr::Constructor {
                                name: "Err".into(),
                                args: vec![CExpr::Var { name: "e".into() }],
                            }),
                        },
                    },
                ],
            }
        }
        s::Expr::Field { value, field } => CExpr::FieldAccess {
            value: Box::new(canonicalize_expr(value)),
            field: field.clone(),
        },
        s::Expr::BinOp { op, lhs, rhs } => CExpr::BinOp {
            op: op.as_str().to_string(),
            lhs: Box::new(canonicalize_expr(lhs)),
            rhs: Box::new(canonicalize_expr(rhs)),
        },
        s::Expr::UnaryOp { op, expr } => CExpr::UnaryOp {
            op: match op { s::UnaryOp::Neg => "-", s::UnaryOp::Not => "not" }.into(),
            expr: Box::new(canonicalize_expr(expr)),
        },
        s::Expr::If { cond, then_block, else_block } => CExpr::Match {
            scrutinee: Box::new(canonicalize_expr(cond)),
            arms: vec![
                Arm {
                    pattern: Pattern::PLiteral { value: CLit::Bool { value: true } },
                    body: canonicalize_block(then_block),
                },
                Arm {
                    pattern: Pattern::PLiteral { value: CLit::Bool { value: false } },
                    body: canonicalize_block(else_block),
                },
            ],
        },
        s::Expr::Match { scrutinee, arms } => CExpr::Match {
            scrutinee: Box::new(canonicalize_expr(scrutinee)),
            arms: arms.iter().map(canonicalize_arm).collect(),
        },
        s::Expr::RecordLit(fields) => {
            let mut fs: Vec<RecordField> = fields
                .iter()
                .map(|f| RecordField {
                    name: f.name.clone(),
                    value: canonicalize_expr(&f.value),
                })
                .collect();
            fs.sort_by(|a, b| a.name.cmp(&b.name));
            CExpr::RecordLit { fields: fs }
        }
        s::Expr::TupleLit(items) => CExpr::TupleLit {
            items: items.iter().map(canonicalize_expr).collect(),
        },
        s::Expr::ListLit(items) => CExpr::ListLit {
            items: items.iter().map(canonicalize_expr).collect(),
        },
        s::Expr::Constructor { name, args } => CExpr::Constructor {
            name: name.clone(),
            args: args.iter().map(canonicalize_expr).collect(),
        },
        s::Expr::Lambda(l) => CExpr::Lambda {
            params: l.params.iter().map(|p| Param {
                name: p.name.clone(),
                ty: canonicalize_type(&p.ty),
            }).collect(),
            return_type: canonicalize_type(&l.return_type),
            effects: {
                let mut es: Vec<Effect> = l.effects.iter().map(canonicalize_effect).collect();
                es.sort_by(|a, b| a.name.cmp(&b.name));
                es
            },
            body: Box::new(canonicalize_block(&l.body)),
        },
    }
}

fn canonicalize_arm(a: &s::Arm) -> Arm {
    Arm { pattern: canonicalize_pattern(&a.pattern), body: canonicalize_expr(&a.body) }
}

fn canonicalize_pattern(p: &s::Pattern) -> Pattern {
    match p {
        s::Pattern::Lit(l) => Pattern::PLiteral { value: canonicalize_lit(l) },
        s::Pattern::Var(name) => {
            // Uppercase-named bare ident in pattern position is a tag-only constructor.
            if is_constructor_name(name) {
                Pattern::PConstructor { name: name.clone(), args: Vec::new() }
            } else {
                Pattern::PVar { name: name.clone() }
            }
        }
        s::Pattern::Wild => Pattern::PWild,
        s::Pattern::Constructor { name, args } => Pattern::PConstructor {
            name: name.clone(),
            args: args.iter().map(canonicalize_pattern).collect(),
        },
        s::Pattern::Record { fields, rest: _ } => {
            let mut fs: Vec<PatternRecordField> = fields
                .iter()
                .map(|f| PatternRecordField {
                    name: f.name.clone(),
                    pattern: f
                        .pattern
                        .as_ref()
                        .map(canonicalize_pattern)
                        .unwrap_or(Pattern::PVar { name: f.name.clone() }),
                })
                .collect();
            fs.sort_by(|a, b| a.name.cmp(&b.name));
            Pattern::PRecord { fields: fs }
        }
        s::Pattern::Tuple(items) => Pattern::PTuple {
            items: items.iter().map(canonicalize_pattern).collect(),
        },
    }
}

fn canonicalize_lit(l: &s::Literal) -> CLit {
    match l {
        s::Literal::Int(n) => CLit::Int { value: *n },
        s::Literal::Float(f) => CLit::Float { value: format_canonical_float(*f) },
        s::Literal::Str(s) => CLit::Str { value: s.clone() },
        s::Literal::Bytes(b) => CLit::Bytes { value: hex(b) },
        s::Literal::Bool(b) => CLit::Bool { value: *b },
        s::Literal::Unit => CLit::Unit,
    }
}

fn format_canonical_float(f: f64) -> String {
    // RFC 8785-flavored: shortest round-trippable. We use Rust's default
    // float formatting, which is already shortest-round-trip via Grisu/Ryu.
    if f.is_finite() {
        let s = format!("{}", f);
        if !s.contains('.') && !s.contains('e') && !s.contains('E') {
            // Ensure float-shape stays a float.
            format!("{}.0", s)
        } else {
            s
        }
    } else if f.is_nan() {
        "NaN".into()
    } else if f.is_sign_positive() {
        "Infinity".into()
    } else {
        "-Infinity".into()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn is_constructor_name(name: &str) -> bool {
    name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
}
