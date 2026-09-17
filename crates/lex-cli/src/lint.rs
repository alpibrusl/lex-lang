//! `lex check --strict` lint passes.
//!
//! These checks catch patterns the type-checker accepts but the runtime
//! may mishandle — the "type-checker accepts, runtime rejects" gap
//! described in the agent-experience review (issue #347, item A2).
//!
//! Four categories:
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
//!
//! 3. **NET_SERVE_NAMED** — a call to one of the name-based `net.serve*`
//!    entry points (`serve`, `serve_tls`, `serve_ws`, `serve_with`,
//!    `serve_quic`), which take the request/message handler as a `Str`
//!    looked up by name at runtime. The handler's effect row is then
//!    invisible to the checker, so effects performed inside it never
//!    propagate to the policy gate (#680). Prefer the effect-polymorphic
//!    closure variants (`serve_fn` / `serve_routed` / `serve_ws_fn` / …),
//!    which thread the handler's effects to the call site.
//!
//! Separately from the lints, this module reports DEPRECATIONS
//! ([`deprecations`]). They are advisory and must never fail a build:
//! `lex check --strict` exits 1 on a lint warning and `lex ci` runs it, so
//! a deprecation folded into the lint set would redden every repository
//! still using the old call — which is all of them, and is the state the
//! notice exists to change rather than punish.

use lex_syntax::syntax::*;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LintWarning {
    pub code: &'static str,
    pub message: String,
    pub location: String,
}

/// The name-based `net.serve*` ops whose handler is a runtime-looked-up
/// `Str`, paired with the closure-based variant to steer callers toward.
const NAMED_SERVE_OPS: &[(&str, &str)] = &[
    ("serve",      "serve_fn / serve_routed"),
    ("serve_tls",  "serve_fn (TLS via ServeOpts) / serve_quic_fn"),
    ("serve_ws",   "serve_ws_fn"),
    ("serve_with", "serve_fn_with / serve_routed_with"),
    ("serve_quic", "serve_quic_fn / serve_quic_routed"),
];

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

    // Aliases bound to `std.net` (usually `net`, but the import can rename
    // it). Used to scope the NET_SERVE_NAMED lint so a user's own `.serve`
    // field access on an unrelated value isn't flagged.
    let net_aliases: Vec<String> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Import(imp) if imp.reference == "std.net" => Some(imp.alias.clone()),
            _ => None,
        })
        .collect();

    let ctx = LintCtx { top_fns: &top_level_fns, net_aliases: &net_aliases };

    for item in &prog.items {
        if let Item::FnDecl(fd) = item {
            for param in &fd.params {
                if ctx.top_fns.contains(&param.name) {
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
            lint_block(&fd.body, &fd.name, &ctx, &mut warnings);
        }
    }

    warnings
}

/// Immutable context threaded through the lint walk.
struct LintCtx<'a> {
    top_fns: &'a [String],
    net_aliases: &'a [String],
}

fn lint_block(block: &Block, fn_name: &str, ctx: &LintCtx, out: &mut Vec<LintWarning>) {
    for stmt in &block.statements {
        match stmt {
            Statement::Let { name, value, .. } => {
                if ctx.top_fns.contains(name) {
                    out.push(LintWarning {
                        code: "SHADOW_FN",
                        message: format!(
                            "`let {name}` shadows top-level function of the same name"
                        ),
                        location: format!("in fn `{fn_name}`"),
                    });
                }
                lint_expr(value, fn_name, ctx, out);
            }
            Statement::Expr(e) => lint_expr(e, fn_name, ctx, out),
        }
    }
    lint_expr(&block.result, fn_name, ctx, out);
}

fn lint_expr(expr: &Expr, fn_name: &str, ctx: &LintCtx, out: &mut Vec<LintWarning>) {
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
            lint_expr(lhs, fn_name, ctx, out);
            lint_expr(rhs, fn_name, ctx, out);
        }
        Expr::Block(b) => lint_block(b, fn_name, ctx, out),
        Expr::Call { callee, args } => {
            if let Some(w) = named_serve_warning(callee, ctx, fn_name) {
                out.push(w);
            }
            lint_expr(callee, fn_name, ctx, out);
            for a in args {
                lint_expr(a, fn_name, ctx, out);
            }
        }
        Expr::Pipe { left, right } => {
            lint_expr(left, fn_name, ctx, out);
            lint_expr(right, fn_name, ctx, out);
        }
        Expr::Try(e) => lint_expr(e, fn_name, ctx, out),
        Expr::Field { value, .. } => lint_expr(value, fn_name, ctx, out),
        Expr::UnaryOp { expr, .. } => lint_expr(expr, fn_name, ctx, out),
        Expr::If { cond, then_block, else_block } => {
            lint_expr(cond, fn_name, ctx, out);
            lint_block(then_block, fn_name, ctx, out);
            lint_block(else_block, fn_name, ctx, out);
        }
        Expr::Match { scrutinee, arms } => {
            lint_expr(scrutinee, fn_name, ctx, out);
            for arm in arms {
                lint_expr(&arm.body, fn_name, ctx, out);
            }
        }
        Expr::RecordLit(fields) => {
            for f in fields {
                lint_expr(&f.value, fn_name, ctx, out);
            }
        }
        Expr::TupleLit(es) | Expr::ListLit(es) => {
            for e in es {
                lint_expr(e, fn_name, ctx, out);
            }
        }
        Expr::Constructor { args, .. } => {
            for a in args {
                lint_expr(a, fn_name, ctx, out);
            }
        }
        Expr::Lambda(lam) => {
            for param in &lam.params {
                if ctx.top_fns.contains(&param.name) {
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
            lint_block(&lam.body, fn_name, ctx, out);
        }
        Expr::Ascription { value, .. } => lint_expr(value, fn_name, ctx, out),
        Expr::Lit(_) | Expr::Var(_) => {}
    }
}

/// If `callee` is `<net_alias>.<named_serve_op>`, return the
/// NET_SERVE_NAMED warning steering the caller to the typed variant.
fn named_serve_warning(callee: &Expr, ctx: &LintCtx, fn_name: &str) -> Option<LintWarning> {
    let Expr::Field { value, field } = callee else { return None; };
    let Expr::Var(alias) = value.as_ref() else { return None; };
    if !ctx.net_aliases.iter().any(|a| a == alias) {
        return None;
    }
    let (_, suggested) = NAMED_SERVE_OPS.iter().find(|(op, _)| op == field)?;
    Some(LintWarning {
        code: "NET_SERVE_NAMED",
        message: format!(
            "`{alias}.{field}` passes the handler by name (looked up at runtime); \
             its effect row is not type-checked or propagated to the policy gate — \
             prefer the closure-based variant ({suggested}) so handler effects are \
             visible and audited"
        ),
        location: format!("in fn `{fn_name}`"),
    })
}

fn is_str_lit(e: &Expr) -> bool {
    matches!(e, Expr::Lit(Literal::Str(_)))
}

// ── deprecations (advisory; never fail a build) ──────────────────────────────

/// `std.io` content ops that `std.fs` replaced, with what to use instead and
/// the effect row that replacement declares (#882).
const DEPRECATED_IO_OPS: &[(&str, &str, &str)] = &[
    ("read", "fs.read_to_string", "fs_read"),
    ("write", "fs.write", "fs_write"),
];

/// Deprecation notices for a program.
///
/// Kept apart from [`lint_program`] because the two have opposite
/// consequences. A lint names a correctness hazard and exits 1. A deprecation
/// names a migration that has not happened yet, and failing on it would break
/// 122 files using `io.read` and 95 using `io.write` across the fleet — the
/// deprecation was announced in a Rust source comment no Lex author ever
/// reads, so nobody has been told to stop.
///
/// The message carries the migration hazard rather than just the replacement,
/// because swapping the call is not the whole change: `fs.read_to_string`
/// declares `[fs_read]` where `io.read` declares `[io]`, so the enclosing
/// function's row changes and that propagates to every caller and every
/// unpinned dependent (#756). A notice that said only "use fs.read_to_string"
/// would send people into that without warning.
pub fn deprecations(prog: &Program) -> Vec<LintWarning> {
    let io_aliases: Vec<String> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Import(imp) if imp.reference == "std.io" => Some(imp.alias.clone()),
            _ => None,
        })
        .collect();
    if io_aliases.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let ctx = DeprecationCtx { io_aliases: &io_aliases };
    for item in &prog.items {
        if let Item::FnDecl(fd) = item {
            walk_block_for_deprecations(&fd.body, &fd.name, &ctx, &mut out);
        }
    }
    out
}

struct DeprecationCtx<'a> {
    io_aliases: &'a [String],
}

fn walk_block_for_deprecations(
    block: &Block,
    fn_name: &str,
    ctx: &DeprecationCtx,
    out: &mut Vec<LintWarning>,
) {
    for stmt in &block.statements {
        match stmt {
            Statement::Let { value, .. } => walk_expr_for_deprecations(value, fn_name, ctx, out),
            Statement::Expr(e) => walk_expr_for_deprecations(e, fn_name, ctx, out),
        }
    }
    walk_expr_for_deprecations(&block.result, fn_name, ctx, out);
}

fn walk_expr_for_deprecations(
    expr: &Expr,
    fn_name: &str,
    ctx: &DeprecationCtx,
    out: &mut Vec<LintWarning>,
) {
    match expr {
        Expr::Call { callee, args } => {
            if let Some(w) = deprecated_io_notice(callee, ctx, fn_name) {
                out.push(w);
            }
            walk_expr_for_deprecations(callee, fn_name, ctx, out);
            for a in args {
                walk_expr_for_deprecations(a, fn_name, ctx, out);
            }
        }
        Expr::Block(b) => walk_block_for_deprecations(b, fn_name, ctx, out),
        Expr::BinOp { lhs, rhs, .. } => {
            walk_expr_for_deprecations(lhs, fn_name, ctx, out);
            walk_expr_for_deprecations(rhs, fn_name, ctx, out);
        }
        Expr::Pipe { left, right } => {
            walk_expr_for_deprecations(left, fn_name, ctx, out);
            walk_expr_for_deprecations(right, fn_name, ctx, out);
        }
        Expr::Try(e) => walk_expr_for_deprecations(e, fn_name, ctx, out),
        Expr::Field { value, .. } => walk_expr_for_deprecations(value, fn_name, ctx, out),
        Expr::UnaryOp { expr, .. } => walk_expr_for_deprecations(expr, fn_name, ctx, out),
        Expr::If { cond, then_block, else_block } => {
            walk_expr_for_deprecations(cond, fn_name, ctx, out);
            walk_block_for_deprecations(then_block, fn_name, ctx, out);
            walk_block_for_deprecations(else_block, fn_name, ctx, out);
        }
        Expr::Match { scrutinee, arms } => {
            walk_expr_for_deprecations(scrutinee, fn_name, ctx, out);
            for arm in arms {
                walk_expr_for_deprecations(&arm.body, fn_name, ctx, out);
            }
        }
        _ => {}
    }
}

fn deprecated_io_notice(
    callee: &Expr,
    ctx: &DeprecationCtx,
    fn_name: &str,
) -> Option<LintWarning> {
    let Expr::Field { value, field } = callee else { return None; };
    let Expr::Var(alias) = value.as_ref() else { return None; };
    if !ctx.io_aliases.iter().any(|a| a == alias) {
        return None;
    }
    let (_, replacement, row) = DEPRECATED_IO_OPS.iter().find(|(op, _, _)| op == field)?;
    Some(LintWarning {
        code: "DEPRECATED_IO",
        message: format!(
            "`{alias}.{field}` takes a path but declares [io], so an effect row \
             never reveals filesystem reach (#882) — use `{replacement}`. \
             NOTE: the replacement declares [{row}], so the enclosing row changes \
             and that propagates to callers and unpinned dependents (#756); \
             migrate a package and its dependents together"
        ),
        location: format!("in fn `{fn_name}`"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lex_syntax::parse_source;

    fn codes(src: &str) -> Vec<&'static str> {
        let prog = parse_source(src).expect("parse");
        lint_program(&prog).into_iter().map(|w| w.code).collect()
    }

    fn dep_codes(src: &str) -> Vec<&'static str> {
        let prog = parse_source(src).expect("parse");
        deprecations(&prog).into_iter().map(|w| w.code).collect()
    }

    fn dep_messages(src: &str) -> Vec<String> {
        let prog = parse_source(src).expect("parse");
        deprecations(&prog).into_iter().map(|w| w.message).collect()
    }

    #[test]
    fn named_serve_is_flagged() {
        let src = r#"
import "std.net" as net
fn main() -> [net] Unit { net.serve(8080, "handle") }
"#;
        assert!(codes(src).contains(&"NET_SERVE_NAMED"),
            "net.serve(port, name) should warn NET_SERVE_NAMED");
    }

    #[test]
    fn other_named_serve_ops_are_flagged() {
        for op in ["serve_tls", "serve_ws", "serve_with", "serve_quic"] {
            let src = format!(
                "import \"std.net\" as net\nfn main() -> [net] Unit {{ net.{op}(8080, \"h\") }}\n");
            assert!(codes(&src).contains(&"NET_SERVE_NAMED"),
                "net.{op} should warn NET_SERVE_NAMED");
        }
    }

    #[test]
    fn closure_based_serve_is_not_flagged() {
        // serve_fn takes a closure, not a name — must not be flagged.
        let src = r#"
import "std.net" as net
fn main() -> [net] Unit {
  net.serve_fn(8080, fn (req :: Request) -> Response { req })
}
"#;
        assert!(!codes(src).contains(&"NET_SERVE_NAMED"),
            "net.serve_fn must not warn");
    }

    #[test]
    fn serve_on_non_net_alias_is_not_flagged() {
        // `.serve` on something not bound to std.net must not be flagged.
        let src = r#"
import "std.http" as http
fn main() -> [net] Unit { http.serve(8080, "h") }
"#;
        assert!(!codes(src).contains(&"NET_SERVE_NAMED"),
            "only the std.net alias should be linted");
    }

    #[test]
    fn respects_import_alias_rename() {
        // The lint follows the alias, not the literal name `net`.
        let src = r#"
import "std.net" as web
fn main() -> [net] Unit { web.serve(8080, "h") }
"#;
        assert!(codes(src).contains(&"NET_SERVE_NAMED"),
            "renamed std.net alias should still be linted");
    }

    // ── deprecations ─────────────────────────────────────────────────────────

    #[test]
    fn io_read_and_write_are_reported() {
        let src = r#"
import "std.io" as io
fn r(p :: Str) -> [io] Str { match io.read(p) { Err(m) => m, Ok(t) => t } }
fn w(p :: Str) -> [io] Str { match io.write(p, "x") { Err(m) => m, Ok(_) => "ok" } }
"#;
        let got = dep_codes(src);
        assert_eq!(got, vec!["DEPRECATED_IO", "DEPRECATED_IO"], "both content ops should be reported");
    }

    // The notice has to carry the migration hazard, not just the replacement.
    // Swapping the call changes the enclosing effect row, which propagates to
    // callers and unpinned dependents (#756) — someone who follows a bare
    // "use fs.read_to_string" walks into that unwarned.
    #[test]
    fn the_notice_names_the_replacement_and_the_row_change() {
        let src = r#"
import "std.io" as io
fn r(p :: Str) -> [io] Str { match io.read(p) { Err(m) => m, Ok(t) => t } }
"#;
        let m = dep_messages(src).remove(0);
        assert!(m.contains("fs.read_to_string"), "must name the replacement: {m}");
        assert!(m.contains("fs_read"), "must name the effect the replacement declares: {m}");
        assert!(m.contains("dependents"), "must warn that the row change propagates: {m}");
    }

    // `io.print` is not deprecated — it is console output, which is what the
    // `[io]` row honestly describes. Flagging it would make the notice noise.
    #[test]
    fn io_print_is_not_deprecated() {
        let src = r#"
import "std.io" as io
fn main() -> [io] Unit { io.print("hello") }
"#;
        assert!(dep_codes(src).is_empty(), "io.print is not a filesystem op");
    }

    #[test]
    fn the_fs_replacements_are_not_reported() {
        let src = r#"
import "std.fs" as fs
fn r(p :: Str) -> [fs_read] Str { match fs.read_to_string(p) { Err(m) => m, Ok(t) => t } }
"#;
        assert!(dep_codes(src).is_empty(), "migrated code must be quiet");
    }

    // Scoped to the alias actually bound to `std.io`, the way NET_SERVE_NAMED
    // scopes to `std.net`. A record field that happens to be called `read` on
    // a value that happens to be called `io` is not a stdlib call.
    #[test]
    fn a_lookalike_field_access_is_not_reported() {
        let src = r#"
import "std.str" as str
type Src = { read :: (Str) -> Str }
fn f(io :: Src, p :: Str) -> Str { io.read(p) }
"#;
        assert!(dep_codes(src).is_empty(), "only calls through a std.io alias count");
    }

    // The import can rename the module; the notice must follow the alias.
    #[test]
    fn a_renamed_import_is_still_reported() {
        let src = r#"
import "std.io" as stdio
fn r(p :: Str) -> [io] Str { match stdio.read(p) { Err(m) => m, Ok(t) => t } }
"#;
        assert!(dep_codes(src).contains(&"DEPRECATED_IO"), "alias may be renamed");
    }

    // Deprecations must not leak into the lint set: `lex check --strict` exits
    // 1 on a lint warning and `lex ci` runs it, so a deprecation counted as a
    // lint would redden every repository that has not migrated — which is
    // nearly all of them.
    #[test]
    fn deprecations_are_not_lint_warnings() {
        let src = r#"
import "std.io" as io
fn r(p :: Str) -> [io] Str { match io.read(p) { Err(m) => m, Ok(t) => t } }
"#;
        assert!(codes(src).is_empty(), "a deprecation must not fail --strict");
        assert!(!dep_codes(src).is_empty(), "but it must still be reported");
    }
}
