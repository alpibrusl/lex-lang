//! Lambda capture analysis: the free variables of an expression that
//! are not bound inside it, in first-reference order. `FnCompiler`
//! (see `super`) uses this to decide which enclosing locals a
//! `MakeClosure` must capture.

use lex_ast as a;

/// Collect free variables referenced in `e` that are not in `bound`.
/// Mutates `bound` to track let/lambda introductions during the walk;
/// the caller's set is preserved on return because Rust's borrow rules
/// force us to clone for sub-scopes that rebind a name.
pub(super) fn free_vars(e: &a::CExpr, bound: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
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
