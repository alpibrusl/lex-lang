//! `lex check --strict` lint passes.
//!
//! These checks catch patterns the type-checker accepts but the runtime
//! may mishandle — the "type-checker accepts, runtime rejects" gap
//! described in the agent-experience review (issue #347, item A2).
//!
//! Three categories:
//!
//! 1. **STR_CMP** — ordering operators (`<`, `<=`, `>`, `>=`) applied to
//!    string literals. The operators are defined for `Str` (lexicographic),
//!    but callers almost always want a semantic ordering (e.g. numeric
//!    parse) and should use `str.compare` or cast first.
//!
//! 2. **SHADOW_FN** — a function parameter or `let` binding whose name
//!    matches a top-level function. Inside the binding's scope the bare
//!    name resolves to the local value, silently shadowing the function.
//!    This caused real confusion when a parameter named `schema` shadowed
//!    the top-level `schema()` helper (#339).

use lex_syntax::syntax::*;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LintWarning {
    pub code: &'static str,
    pub message: String,
    pub location: String,
}

pub fn lint_program(prog: &Program) -> Vec<LintWarning> {
    let mut warnings = Vec::new();

    let top_level_fns: Vec<String> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FnDecl(fd) => Some(fd.name.clone()),
            _ => None,
        })
        .collect();

    for item in &prog.items {
        if let Item::FnDecl(fd) = item {
            for param in &fd.params {
                if top_level_fns.contains(&param.name) {
                    warnings.push(LintWarning {
                        code: "SHADOW_FN",
                        message: format!(
                            "parameter `{}` shadows top-level function of the same name",
                            param.name
                        ),
                        location: format!("in fn `{}`", fd.name),
                    });
                }
            }
            lint_block(&fd.body, &fd.name, &top_level_fns, &mut warnings);
        }
    }

    warnings
}

fn lint_block(block: &Block, fn_name: &str, top_fns: &[String], out: &mut Vec<LintWarning>) {
    for stmt in &block.statements {
        match stmt {
            Statement::Let { name, value, .. } => {
                if top_fns.contains(name) {
                    out.push(LintWarning {
                        code: "SHADOW_FN",
                        message: format!(
                            "`let {name}` shadows top-level function of the same name"
                        ),
                        location: format!("in fn `{fn_name}`"),
                    });
                }
                lint_expr(value, fn_name, top_fns, out);
            }
            Statement::Expr(e) => lint_expr(e, fn_name, top_fns, out),
        }
    }
    lint_expr(&block.result, fn_name, top_fns, out);
}

fn lint_expr(expr: &Expr, fn_name: &str, top_fns: &[String], out: &mut Vec<LintWarning>) {
    match expr {
        Expr::BinOp { op, lhs, rhs } => {
            if matches!(op, BinOp::Lt | BinOp::Lte | BinOp::Gt | BinOp::Gte)
                && (is_str_lit(lhs) || is_str_lit(rhs))
            {
                out.push(LintWarning {
                    code: "STR_CMP",
                    message: format!(
                        "operator `{}` applied to a Str literal; \
                         ordering on Str is lexicographic — use `str.compare` \
                         or cast to Int if you want numeric ordering",
                        op.as_str()
                    ),
                    location: format!("in fn `{fn_name}`"),
                });
            }
            lint_expr(lhs, fn_name, top_fns, out);
            lint_expr(rhs, fn_name, top_fns, out);
        }
        Expr::Block(b) => lint_block(b, fn_name, top_fns, out),
        Expr::Call { callee, args } => {
            lint_expr(callee, fn_name, top_fns, out);
            for a in args {
                lint_expr(a, fn_name, top_fns, out);
            }
        }
        Expr::Pipe { left, right } => {
            lint_expr(left, fn_name, top_fns, out);
            lint_expr(right, fn_name, top_fns, out);
        }
        Expr::Try(e) => lint_expr(e, fn_name, top_fns, out),
        Expr::Field { value, .. } => lint_expr(value, fn_name, top_fns, out),
        Expr::UnaryOp { expr, .. } => lint_expr(expr, fn_name, top_fns, out),
        Expr::If { cond, then_block, else_block } => {
            lint_expr(cond, fn_name, top_fns, out);
            lint_block(then_block, fn_name, top_fns, out);
            lint_block(else_block, fn_name, top_fns, out);
        }
        Expr::Match { scrutinee, arms } => {
            lint_expr(scrutinee, fn_name, top_fns, out);
            for arm in arms {
                lint_expr(&arm.body, fn_name, top_fns, out);
            }
        }
        Expr::RecordLit(fields) => {
            for f in fields {
                lint_expr(&f.value, fn_name, top_fns, out);
            }
        }
        Expr::TupleLit(es) | Expr::ListLit(es) => {
            for e in es {
                lint_expr(e, fn_name, top_fns, out);
            }
        }
        Expr::Constructor { args, .. } => {
            for a in args {
                lint_expr(a, fn_name, top_fns, out);
            }
        }
        Expr::Lambda(lam) => {
            for param in &lam.params {
                if top_fns.contains(&param.name) {
                    out.push(LintWarning {
                        code: "SHADOW_FN",
                        message: format!(
                            "lambda parameter `{}` shadows top-level function of the same name",
                            param.name
                        ),
                        location: format!("in fn `{fn_name}`"),
                    });
                }
            }
            lint_block(&lam.body, fn_name, top_fns, out);
        }
        Expr::Ascription { value, .. } => lint_expr(value, fn_name, top_fns, out),
        Expr::Lit(_) | Expr::Var(_) => {}
    }
}

fn is_str_lit(e: &Expr) -> bool {
    matches!(e, Expr::Lit(Literal::Str(_)))
}
