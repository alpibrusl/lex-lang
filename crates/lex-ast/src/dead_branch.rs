//! Dead-branch elimination on the canonical AST (#228).
//!
//! Folds `match LITERAL { ... }` to whichever arm matches the scrutinee.
//! In particular this catches `if true { ... } else { ... }` and
//! `if false { ... } else { ... }`, since the canonicalizer desugars
//! both into `match` over a Bool literal.
//!
//! Why bother?
//!
//! - **Effect-set soundness, not just perf.** A function whose only
//!   `[net]` usage lives in a dead branch should not have `[net]` in
//!   its inferred effect set. Without this pass, the type-checker
//!   walks both arms of the match and pulls in the dead branch's
//!   effects, which inflates the function's signature against the
//!   capability gate. Running this pass *before* type-checking lets
//!   the inferred set reflect only live code.
//!
//! - **Cleaner trace output.** The bytecode never emits the dead
//!   branch, so the trace doesn't carry a stale "this branch could
//!   have run" hint.
//!
//! Single-pass / depth-first / no fixpoint loop. The recursive walk
//! folds children before attempting to fold the current `Match`, so
//! chained conditionals (`if true { if false { a } else { b } }`)
//! reduce to `b` in one traversal.

use crate::canonical::*;

/// Walk every `Stage::FnDecl`'s body, folding dead branches.
/// Stages that aren't function decls (type aliases, imports) are
/// passed through unchanged.
pub fn eliminate_dead_branches_in_stages(stages: Vec<Stage>) -> Vec<Stage> {
    stages.into_iter().map(eliminate_in_stage).collect()
}

fn eliminate_in_stage(stage: Stage) -> Stage {
    match stage {
        Stage::FnDecl(fd) => Stage::FnDecl(FnDecl {
            body: fold(fd.body),
            ..fd
        }),
        other => other,
    }
}

/// Recursive depth-first fold over `CExpr`. Folds children first;
/// after the children are reduced, attempts the dead-branch rewrite
/// at the current node.
pub fn fold(e: CExpr) -> CExpr {
    let folded = match e {
        CExpr::Match { scrutinee, arms } => {
            let s = fold(*scrutinee);
            let arms: Vec<Arm> = arms.into_iter()
                .map(|a| Arm { pattern: a.pattern, body: fold(a.body) })
                .collect();
            CExpr::Match { scrutinee: Box::new(s), arms }
        }
        CExpr::Call { callee, args } => CExpr::Call {
            callee: Box::new(fold(*callee)),
            args: args.into_iter().map(fold).collect(),
        },
        CExpr::Let { name, ty, value, body } => CExpr::Let {
            name, ty,
            value: Box::new(fold(*value)),
            body: Box::new(fold(*body)),
        },
        CExpr::Block { statements, result } => CExpr::Block {
            statements: statements.into_iter().map(fold).collect(),
            result: Box::new(fold(*result)),
        },
        CExpr::Constructor { name, args } => CExpr::Constructor {
            name,
            args: args.into_iter().map(fold).collect(),
        },
        CExpr::RecordLit { fields } => CExpr::RecordLit {
            fields: fields.into_iter().map(|f| RecordField {
                name: f.name,
                value: fold(f.value),
            }).collect(),
        },
        CExpr::TupleLit { items } => CExpr::TupleLit {
            items: items.into_iter().map(fold).collect(),
        },
        CExpr::ListLit { items } => CExpr::ListLit {
            items: items.into_iter().map(fold).collect(),
        },
        CExpr::FieldAccess { value, field } => CExpr::FieldAccess {
            value: Box::new(fold(*value)),
            field,
        },
        CExpr::Lambda { params, return_type, effects, body } => CExpr::Lambda {
            params, return_type, effects,
            body: Box::new(fold(*body)),
        },
        CExpr::BinOp { op, lhs, rhs } => CExpr::BinOp {
            op,
            lhs: Box::new(fold(*lhs)),
            rhs: Box::new(fold(*rhs)),
        },
        CExpr::UnaryOp { op, expr } => CExpr::UnaryOp {
            op,
            expr: Box::new(fold(*expr)),
        },
        CExpr::Return { value } => CExpr::Return {
            value: Box::new(fold(*value)),
        },
        // Literal, Var: leaves
        leaf => leaf,
    };

    // After children are folded, try the rewrite at this level.
    if let CExpr::Match { scrutinee, arms } = &folded {
        if let CExpr::Literal { value: lit } = scrutinee.as_ref() {
            for arm in arms {
                if pattern_matches_literal(&arm.pattern, lit) {
                    return arm.body.clone();
                }
            }
        }
    }
    folded
}

/// Whether `pat` definitely matches a value of literal `lit`.
/// Conservative: only structural-literal and wildcard patterns are
/// folded. Var patterns *do* match any value but folding them would
/// require substituting the bound name through the body — out of
/// scope for this v1.
fn pattern_matches_literal(pat: &Pattern, lit: &CLit) -> bool {
    match pat {
        Pattern::PLiteral { value } => value == lit,
        Pattern::PWild => true,
        _ => false,
    }
}
