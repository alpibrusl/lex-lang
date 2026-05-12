//! Typed, valid-by-construction AST transforms (#280).
//!
//! The op log historically records body-shaping edits as opaque
//! `ModifyBody { from_stage_id, to_stage_id, … }` — the bytes
//! changed, but *what* changed is recoverable only by diffing the
//! two stages. For agents writing Lex, that's the wrong primitive:
//! they want to express "replace the body of arm 2 in this match"
//! as a single typed operation, get back a valid AST, and have the
//! op log record the transform's intent, not just its byte effect.
//!
//! Each transform in this module is a pure function:
//!
//!   `fn transform(stage: &Stage, params…) -> Result<Stage, TransformError>`
//!
//! No I/O, no LLM, no type checking — type checking happens *after*
//! the transform in `lex-store`'s apply path, so a transform that
//! produces an ill-typed AST surfaces as `StoreError::TypeError`
//! rather than failing inside the transformer.
//!
//! # Shipped transforms
//!
//! - [`replace_match_arm`] — replaces the body of one arm in a
//!   match expression. Pattern is preserved.
//! - [`rename_local`] — renames a `let`-bound local. Walks the
//!   binding's body scope and rewrites every unshadowed reference.
//! - [`inline_let`] — eliminates a `let x := v; body` by
//!   substituting `v` for every unshadowed `x` in `body`. The let
//!   value is restricted to capture-free, side-effect-free
//!   expressions (literals, vars, field access, binops on those).
//! - [`extract_function`] — extracts a sub-expression from a fn
//!   body into a new top-level function, replacing the original
//!   site with a call. The agent provides the new fn's signature
//!   (params, return type, effects); the transform verifies free
//!   variables match the params.

use crate::canonical::{
    CExpr, Effect, FnDecl, Param, Pattern, Stage, TypeExpr,
};
use crate::ids::NodeId;

#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransformError {
    #[error("unknown node id `{at}`")]
    UnknownNode { at: String },
    #[error("expected a Match expression at `{at}` but found `{found_kind}`")]
    NotAMatch { at: String, found_kind: &'static str },
    #[error("expected a Let expression at `{at}` but found `{found_kind}`")]
    NotALet { at: String, found_kind: &'static str },
    #[error("arm index {requested} out of range (arm count = {arm_count}) at `{at}`")]
    ArmIndexOutOfRange { at: String, arm_count: usize, requested: usize },
    #[error("malformed NodeId `{0}`")]
    BadNodeId(String),
    #[error("cannot transform inside `{stage_kind}` — only FnDecl bodies are transformable")]
    NonFnTarget { stage_kind: &'static str },
    #[error("rename is a no-op: old and new name are both `{name}`")]
    RenameNoOp { name: String },
    #[error("inline_let refused: `{reason}`")]
    InlineLetRefused { reason: String },
    #[error("extract_function refused: `{reason}`")]
    ExtractFnRefused { reason: String },
}

/// Specification of the new function that [`extract_function`]
/// produces. The agent provides this — slice 4 doesn't infer
/// types or effects from the surrounding context. The transform
/// verifies the param set matches the extracted expression's free
/// variables; the type-checker (downstream) catches mismatches in
/// types, effects, and return type.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractFnSpec {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub return_type: TypeExpr,
    pub effects: Vec<Effect>,
}

/// Replace `match_node`'s arm at `arm_index` with a new body,
/// preserving the arm's pattern (#280 slice 1).
///
/// `match_node` must address a `CExpr::Match` inside the FnDecl
/// body of `stage`. The returned `Stage` is a clone with the one
/// arm body replaced; everything else is untouched, so the
/// resulting `StageId` differs from the input's only by the
/// affected sub-tree.
pub fn replace_match_arm(
    stage: &Stage,
    match_node: &NodeId,
    arm_index: usize,
    new_body: CExpr,
) -> Result<Stage, TransformError> {
    let mut out = stage.clone();
    let (body, n_params) = match &mut out {
        Stage::FnDecl(fd) => {
            let n = fd.params.len();
            (&mut fd.body, n)
        }
        Stage::TypeDecl(_) => return Err(TransformError::NonFnTarget { stage_kind: "TypeDecl" }),
        Stage::Import(_) => return Err(TransformError::NonFnTarget { stage_kind: "Import" }),
    };
    let path = parse_node_id(match_node.as_str())?;
    // First index identifies a child of the FnDecl: params at
    // 0..n_params, return_type at n_params, body at n_params + 1.
    // Only the body slot is reachable for this transform.
    if path.is_empty() {
        return Err(TransformError::NotAMatch {
            at: match_node.as_str().into(),
            found_kind: "stage_root",
        });
    }
    if path[0] != n_params + 1 {
        return Err(TransformError::UnknownNode { at: match_node.as_str().into() });
    }
    let inner = &path[1..];
    let target = navigate_to_expr(body, inner, match_node.as_str())?;
    let CExpr::Match { scrutinee: _, arms } = target else {
        return Err(TransformError::NotAMatch {
            at: match_node.as_str().into(),
            found_kind: cexpr_kind(target),
        });
    };
    if arm_index >= arms.len() {
        return Err(TransformError::ArmIndexOutOfRange {
            at: match_node.as_str().into(),
            arm_count: arms.len(),
            requested: arm_index,
        });
    }
    arms[arm_index].body = new_body;
    Ok(out)
}

/// Rename a `let`-bound local variable. Walks the binding's body
/// scope and rewrites every unshadowed reference to the old name.
/// Pure function — produces a new `Stage` with the rename applied.
///
/// Scope semantics:
/// - The let's `value` is evaluated in the outer scope; references
///   to the old name there belong to whatever bound it before.
///   We rewrite the `value` only if a renamed outer binding would
///   reach it — which it can't, since `let` in Lex is
///   non-recursive. So we leave `value` untouched.
/// - The let's `body` sees the renamed binding. Shadowing in `body`
///   (a nested `Let` / `Lambda` param / `Match` pattern that
///   re-binds the *old* name) cuts off renaming inside the
///   shadowed sub-tree.
///
/// Refuses when `old_name == new_name`. Doesn't otherwise check for
/// name clashes — if the new name is already bound in scope, the
/// downstream type-checker surfaces it as a `TypeError`.
pub fn rename_local(
    stage: &Stage,
    let_node: &NodeId,
    new_name: &str,
) -> Result<Stage, TransformError> {
    let mut out = stage.clone();
    let (body, n_params) = match &mut out {
        Stage::FnDecl(fd) => {
            let n = fd.params.len();
            (&mut fd.body, n)
        }
        Stage::TypeDecl(_) => return Err(TransformError::NonFnTarget { stage_kind: "TypeDecl" }),
        Stage::Import(_) => return Err(TransformError::NonFnTarget { stage_kind: "Import" }),
    };
    let path = parse_node_id(let_node.as_str())?;
    if path.is_empty() {
        return Err(TransformError::NotALet {
            at: let_node.as_str().into(),
            found_kind: "stage_root",
        });
    }
    if path[0] != n_params + 1 {
        return Err(TransformError::UnknownNode { at: let_node.as_str().into() });
    }
    let inner = &path[1..];
    let target = navigate_to_expr(body, inner, let_node.as_str())?;
    let CExpr::Let { name, body: let_body, .. } = target else {
        return Err(TransformError::NotALet {
            at: let_node.as_str().into(),
            found_kind: cexpr_kind(target),
        });
    };
    if name == new_name {
        return Err(TransformError::RenameNoOp { name: name.clone() });
    }
    let old_name = std::mem::replace(name, new_name.to_string());
    // Walk the body scope. Stop renaming at shadow points.
    rewrite_var_in_expr(let_body, &old_name, new_name);
    Ok(out)
}

/// Inline a `let x := v; body` by substituting `v` for every
/// unshadowed occurrence of `x` in `body`, then replacing the
/// entire `Let` node with the substituted `body`.
///
/// Restrictions (slice 3):
/// - `v` must be capture-free, side-effect-free, and cheap to
///   duplicate: only `Literal`, `Var`, `FieldAccess`, and `BinOp`/
///   `UnaryOp` trees over those primitives are accepted. Calls,
///   lambdas, blocks, lets, matches in `v` are refused with
///   `InlineLetRefused` so a future slice can lift the restriction
///   without changing the error contract.
/// - None of `v`'s free variables may be re-bound (shadowed)
///   anywhere in `body`. Inlining `let x := y; let y := …; x` is
///   refused because the inner `let y` would capture the inlined
///   `y` and silently change semantics.
///
/// Refuses no-ops at the API boundary: a `let x := v; x` body
/// (single direct reference) is fine; a `let x := v; { }` body
/// with zero references is *not* refused — the let is still
/// eliminated, which is the user-visible point of inlining.
pub fn inline_let(
    stage: &Stage,
    let_node: &NodeId,
) -> Result<Stage, TransformError> {
    let mut out = stage.clone();
    let (body, n_params) = match &mut out {
        Stage::FnDecl(fd) => {
            let n = fd.params.len();
            (&mut fd.body, n)
        }
        Stage::TypeDecl(_) => return Err(TransformError::NonFnTarget { stage_kind: "TypeDecl" }),
        Stage::Import(_) => return Err(TransformError::NonFnTarget { stage_kind: "Import" }),
    };
    let path = parse_node_id(let_node.as_str())?;
    if path.is_empty() {
        return Err(TransformError::NotALet {
            at: let_node.as_str().into(),
            found_kind: "stage_root",
        });
    }
    if path[0] != n_params + 1 {
        return Err(TransformError::UnknownNode { at: let_node.as_str().into() });
    }
    let inner = &path[1..];
    // Special case: when the path is `n_<n+1>` exactly (no further
    // descent), the target is the FnDecl's body root. We swap it
    // for the inlined body wholesale.
    if inner.is_empty() {
        let CExpr::Let { name, value, body: let_body, .. } = body.clone() else {
            return Err(TransformError::NotALet {
                at: let_node.as_str().into(),
                found_kind: cexpr_kind(body),
            });
        };
        check_inlinable(&value)?;
        let captures = free_vars(&value);
        check_no_capture(&let_body, &captures)?;
        let mut replaced = *let_body;
        substitute_in_expr(&mut replaced, &name, &value);
        *body = replaced;
        return Ok(out);
    }
    // Otherwise descend to the parent of the let node, capture the
    // let's owned contents, and splice the inlined body into its
    // slot. We re-use `navigate_to_expr` to reach the let, clone
    // its parts, then mutate the slot in place.
    let target = navigate_to_expr(body, inner, let_node.as_str())?;
    let CExpr::Let { name, value, body: let_body, .. } = target.clone() else {
        return Err(TransformError::NotALet {
            at: let_node.as_str().into(),
            found_kind: cexpr_kind(target),
        });
    };
    check_inlinable(&value)?;
    let captures = free_vars(&value);
    check_no_capture(&let_body, &captures)?;
    let mut replaced = *let_body;
    substitute_in_expr(&mut replaced, &name, &value);
    *target = replaced;
    Ok(out)
}

/// True when `v` is restricted to the slice-3 inline-safe shape:
/// literals, vars, field accesses, binops/unaryops on those. No
/// calls, lambdas, blocks, lets, matches, constructors, record
/// literals, or returns.
fn check_inlinable(v: &CExpr) -> Result<(), TransformError> {
    match v {
        CExpr::Literal { .. } | CExpr::Var { .. } => Ok(()),
        CExpr::FieldAccess { value, .. } => check_inlinable(value),
        CExpr::BinOp { lhs, rhs, .. } => {
            check_inlinable(lhs)?;
            check_inlinable(rhs)
        }
        CExpr::UnaryOp { expr, .. } => check_inlinable(expr),
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for it in items { check_inlinable(it)?; }
            Ok(())
        }
        other => Err(TransformError::InlineLetRefused {
            reason: format!(
                "let value contains a `{}` expression; slice 3 only inlines literal/var/field/binop/unaryop/tuple/list trees",
                cexpr_kind(other)
            ),
        }),
    }
}

/// Collect the set of variable names that appear free in `v`.
/// Order-independent (`BTreeSet` for determinism in tests).
fn free_vars(v: &CExpr) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    collect_free_vars(v, &mut out);
    out
}

fn collect_free_vars(e: &CExpr, out: &mut std::collections::BTreeSet<String>) {
    match e {
        CExpr::Var { name } => { out.insert(name.clone()); }
        CExpr::Literal { .. } => {}
        CExpr::Call { callee, args } => {
            collect_free_vars(callee, out);
            for a in args { collect_free_vars(a, out); }
        }
        CExpr::Let { value, body, name, .. } => {
            collect_free_vars(value, out);
            // Body sees the let binding. We approximate: collect
            // body's free vars, then remove the let's name.
            let mut inner = std::collections::BTreeSet::new();
            collect_free_vars(body, &mut inner);
            inner.remove(name);
            out.extend(inner);
        }
        CExpr::Match { scrutinee, arms } => {
            collect_free_vars(scrutinee, out);
            for arm in arms {
                let mut inner = std::collections::BTreeSet::new();
                collect_free_vars(&arm.body, &mut inner);
                let bound = pattern_bindings(&arm.pattern);
                for b in bound { inner.remove(&b); }
                out.extend(inner);
            }
        }
        CExpr::Block { statements, result } => {
            for s in statements { collect_free_vars(s, out); }
            collect_free_vars(result, out);
        }
        CExpr::Constructor { args, .. } => {
            for a in args { collect_free_vars(a, out); }
        }
        CExpr::RecordLit { fields } => {
            for f in fields { collect_free_vars(&f.value, out); }
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items { collect_free_vars(i, out); }
        }
        CExpr::FieldAccess { value, .. } => collect_free_vars(value, out),
        CExpr::Lambda { params, body, .. } => {
            let mut inner = std::collections::BTreeSet::new();
            collect_free_vars(body, &mut inner);
            for p in params { inner.remove(&p.name); }
            out.extend(inner);
        }
        CExpr::BinOp { lhs, rhs, .. } => {
            collect_free_vars(lhs, out);
            collect_free_vars(rhs, out);
        }
        CExpr::UnaryOp { expr, .. } => collect_free_vars(expr, out),
        CExpr::Return { value } => collect_free_vars(value, out),
    }
}

fn pattern_bindings(p: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    collect_pattern_bindings(p, &mut out);
    out
}

fn collect_pattern_bindings(p: &Pattern, out: &mut Vec<String>) {
    match p {
        Pattern::PVar { name } => out.push(name.clone()),
        Pattern::PLiteral { .. } | Pattern::PWild => {}
        Pattern::PConstructor { args, .. } => for p in args { collect_pattern_bindings(p, out); }
        Pattern::PRecord { fields } => for f in fields { collect_pattern_bindings(&f.pattern, out); }
        Pattern::PTuple { items } => for p in items { collect_pattern_bindings(p, out); }
    }
}

/// Walk `body`; if any sub-tree introduces a binder whose name is
/// in `captures`, refuse the inline. We don't need to track scope
/// here — any shadow at any depth is a problem, because once we
/// inline, that shadow would capture a name in the substituted
/// value.
fn check_no_capture(
    body: &CExpr,
    captures: &std::collections::BTreeSet<String>,
) -> Result<(), TransformError> {
    let mut conflict: Option<String> = None;
    walk_binders(body, &mut |name| {
        if captures.contains(name) && conflict.is_none() {
            conflict = Some(name.to_string());
        }
    });
    if let Some(name) = conflict {
        return Err(TransformError::InlineLetRefused {
            reason: format!(
                "value's free var `{name}` is re-bound in the body; inlining would capture"
            ),
        });
    }
    Ok(())
}

fn walk_binders(e: &CExpr, on_binder: &mut dyn FnMut(&str)) {
    match e {
        CExpr::Let { name, value, body, .. } => {
            on_binder(name);
            walk_binders(value, on_binder);
            walk_binders(body, on_binder);
        }
        CExpr::Lambda { params, body, .. } => {
            for p in params { on_binder(&p.name); }
            walk_binders(body, on_binder);
        }
        CExpr::Match { scrutinee, arms } => {
            walk_binders(scrutinee, on_binder);
            for arm in arms {
                for b in pattern_bindings(&arm.pattern) { on_binder(&b); }
                walk_binders(&arm.body, on_binder);
            }
        }
        CExpr::Call { callee, args } => {
            walk_binders(callee, on_binder);
            for a in args { walk_binders(a, on_binder); }
        }
        CExpr::Block { statements, result } => {
            for s in statements { walk_binders(s, on_binder); }
            walk_binders(result, on_binder);
        }
        CExpr::Constructor { args, .. } => for a in args { walk_binders(a, on_binder); }
        CExpr::RecordLit { fields } => for f in fields { walk_binders(&f.value, on_binder); }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items { walk_binders(i, on_binder); }
        }
        CExpr::FieldAccess { value, .. } => walk_binders(value, on_binder),
        CExpr::BinOp { lhs, rhs, .. } => {
            walk_binders(lhs, on_binder); walk_binders(rhs, on_binder);
        }
        CExpr::UnaryOp { expr, .. } => walk_binders(expr, on_binder),
        CExpr::Return { value } => walk_binders(value, on_binder),
        CExpr::Var { .. } | CExpr::Literal { .. } => {}
    }
}

/// Substitute every unshadowed reference to `name` in `e` with a
/// clone of `replacement`. Shadowing rules match `rewrite_var_in_expr`:
/// inner `Let` / `Lambda param` / `Match pattern` that re-binds
/// `name` cuts off descent.
fn substitute_in_expr(e: &mut CExpr, name: &str, replacement: &CExpr) {
    match e {
        CExpr::Var { name: n } if n == name => {
            *e = replacement.clone();
        }
        CExpr::Var { .. } | CExpr::Literal { .. } => {}
        CExpr::Call { callee, args } => {
            substitute_in_expr(callee, name, replacement);
            for a in args { substitute_in_expr(a, name, replacement); }
        }
        CExpr::Let { name: binder, value, body, .. } => {
            substitute_in_expr(value, name, replacement);
            if binder != name {
                substitute_in_expr(body, name, replacement);
            }
        }
        CExpr::Match { scrutinee, arms } => {
            substitute_in_expr(scrutinee, name, replacement);
            for arm in arms {
                if !pattern_binds(&arm.pattern, name) {
                    substitute_in_expr(&mut arm.body, name, replacement);
                }
            }
        }
        CExpr::Block { statements, result } => {
            for s in statements { substitute_in_expr(s, name, replacement); }
            substitute_in_expr(result, name, replacement);
        }
        CExpr::Constructor { args, .. } => {
            for a in args { substitute_in_expr(a, name, replacement); }
        }
        CExpr::RecordLit { fields } => {
            for f in fields { substitute_in_expr(&mut f.value, name, replacement); }
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items { substitute_in_expr(i, name, replacement); }
        }
        CExpr::FieldAccess { value, .. } => substitute_in_expr(value, name, replacement),
        CExpr::Lambda { params, body, .. } => {
            if !params.iter().any(|p| p.name == name) {
                substitute_in_expr(body, name, replacement);
            }
        }
        CExpr::BinOp { lhs, rhs, .. } => {
            substitute_in_expr(lhs, name, replacement);
            substitute_in_expr(rhs, name, replacement);
        }
        CExpr::UnaryOp { expr, .. } => substitute_in_expr(expr, name, replacement),
        CExpr::Return { value } => substitute_in_expr(value, name, replacement),
    }
}

fn rewrite_var_in_expr(e: &mut CExpr, old: &str, new: &str) {
    match e {
        CExpr::Var { name } => {
            if name == old { *name = new.into(); }
        }
        CExpr::Literal { .. } => {}
        CExpr::Call { callee, args } => {
            rewrite_var_in_expr(callee, old, new);
            for a in args { rewrite_var_in_expr(a, old, new); }
        }
        CExpr::Let { name, value, body, .. } => {
            // `value` is evaluated in the outer scope — rewrite
            // there.
            rewrite_var_in_expr(value, old, new);
            // `body` sees the new binding. If THIS let re-binds
            // `old`, it shadows the rename target; stop descending.
            if name != old {
                rewrite_var_in_expr(body, old, new);
            }
        }
        CExpr::Match { scrutinee, arms } => {
            rewrite_var_in_expr(scrutinee, old, new);
            for arm in arms {
                if !pattern_binds(&arm.pattern, old) {
                    rewrite_var_in_expr(&mut arm.body, old, new);
                }
            }
        }
        CExpr::Block { statements, result } => {
            for s in statements { rewrite_var_in_expr(s, old, new); }
            rewrite_var_in_expr(result, old, new);
        }
        CExpr::Constructor { args, .. } => {
            for a in args { rewrite_var_in_expr(a, old, new); }
        }
        CExpr::RecordLit { fields } => {
            for f in fields { rewrite_var_in_expr(&mut f.value, old, new); }
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items { rewrite_var_in_expr(i, old, new); }
        }
        CExpr::FieldAccess { value, .. } => rewrite_var_in_expr(value, old, new),
        CExpr::Lambda { params, body, .. } => {
            // Lambda params shadow the outer binding.
            if !params.iter().any(|p| p.name == old) {
                rewrite_var_in_expr(body, old, new);
            }
        }
        CExpr::BinOp { lhs, rhs, .. } => {
            rewrite_var_in_expr(lhs, old, new);
            rewrite_var_in_expr(rhs, old, new);
        }
        CExpr::UnaryOp { expr, .. } => rewrite_var_in_expr(expr, old, new),
        CExpr::Return { value } => rewrite_var_in_expr(value, old, new),
    }
}

fn pattern_binds(p: &Pattern, name: &str) -> bool {
    match p {
        Pattern::PVar { name: n } => n == name,
        Pattern::PLiteral { .. } | Pattern::PWild => false,
        Pattern::PConstructor { args, .. } => args.iter().any(|p| pattern_binds(p, name)),
        Pattern::PRecord { fields } => fields.iter().any(|f| pattern_binds(&f.pattern, name)),
        Pattern::PTuple { items } => items.iter().any(|p| pattern_binds(p, name)),
    }
}

/// Extract a sub-expression from a fn body into a new top-level
/// function (#280 slice 4). Returns `(modified_source, new_fn)`:
/// the original stage with the extracted expression replaced by a
/// call to the new fn, and the new fn itself as a fresh `Stage`.
///
/// `expr_node` is the [`NodeId`] of the expression to extract.
/// `spec` describes the new fn's full signature; the transform
/// verifies that `spec.params` covers exactly the free variables
/// of the extracted expression — names must match. Type checking
/// of the extracted body against the spec happens *after* the
/// transform, in the apply path's re-typecheck.
///
/// The replacement call site uses the parameter order from
/// `spec.params`: `f(a, b, c)` for `params = [a, b, c]`.
pub fn extract_function(
    stage: &Stage,
    expr_node: &NodeId,
    spec: ExtractFnSpec,
) -> Result<(Stage, Stage), TransformError> {
    // Locate + clone the expression to extract from a fresh stage
    // copy; we need a separate mutable clone for the modification.
    let mut modified = stage.clone();
    let (body, n_params) = match &mut modified {
        Stage::FnDecl(fd) => {
            let n = fd.params.len();
            (&mut fd.body, n)
        }
        Stage::TypeDecl(_) => return Err(TransformError::NonFnTarget { stage_kind: "TypeDecl" }),
        Stage::Import(_) => return Err(TransformError::NonFnTarget { stage_kind: "Import" }),
    };
    let path = parse_node_id(expr_node.as_str())?;
    if path.is_empty() {
        return Err(TransformError::UnknownNode { at: expr_node.as_str().into() });
    }
    if path[0] != n_params + 1 {
        return Err(TransformError::UnknownNode { at: expr_node.as_str().into() });
    }
    let inner = &path[1..];
    let target = navigate_to_expr(body, inner, expr_node.as_str())?;

    // Snapshot the expression we're about to extract. We need a
    // value, not a reference, because we'll move it into the new
    // fn's body.
    let extracted_expr = target.clone();

    // Free vars of the extracted expr must exactly match the
    // declared param names. If the agent passed extra params or
    // missed any, refuse — the agent did the analysis wrong.
    let free = free_vars(&extracted_expr);
    let declared: std::collections::BTreeSet<String> =
        spec.params.iter().map(|p| p.name.clone()).collect();
    if free != declared {
        let only_in_free: Vec<&String> = free.difference(&declared).collect();
        let only_in_declared: Vec<&String> = declared.difference(&free).collect();
        return Err(TransformError::ExtractFnRefused {
            reason: format!(
                "free vars {free:?} differ from declared params {declared:?}: \
                 missing {only_in_free:?}, extra {only_in_declared:?}"
            ),
        });
    }

    // Replace the extracted expr with a call to the new fn. Args
    // are `Var { name }` for each declared param, in the spec's
    // declared order.
    let call = CExpr::Call {
        callee: Box::new(CExpr::Var { name: spec.name.clone() }),
        args: spec.params.iter()
            .map(|p| CExpr::Var { name: p.name.clone() })
            .collect(),
    };
    *target = call;

    // Build the new fn stage from the extracted body + spec.
    let new_fn = Stage::FnDecl(FnDecl {
        name: spec.name,
        type_params: spec.type_params,
        params: spec.params,
        effects: spec.effects,
        return_type: spec.return_type,
        body: extracted_expr,
        examples: Vec::new(),
    });

    Ok((modified, new_fn))
}

fn parse_node_id(id: &str) -> Result<Vec<usize>, TransformError> {
    let s = id.strip_prefix("n_").ok_or_else(|| TransformError::BadNodeId(id.into()))?;
    let mut parts = s.split('.');
    let head = parts.next().ok_or_else(|| TransformError::BadNodeId(id.into()))?;
    if head != "0" {
        return Err(TransformError::BadNodeId(id.into()));
    }
    let mut out = Vec::new();
    for p in parts {
        out.push(p.parse::<usize>().map_err(|_| TransformError::BadNodeId(id.into()))?);
    }
    Ok(out)
}

/// Walk into an expression tree along a path of child indices and
/// return a `&mut` to the resolved sub-expression. The indexing
/// matches [`crate::ids::collect_ids`]'s walk order so a
/// `NodeId` minted from `collect_ids` resolves to the same node
/// here.
fn navigate_to_expr<'a>(
    root: &'a mut CExpr,
    path: &[usize],
    target_id: &str,
) -> Result<&'a mut CExpr, TransformError> {
    let mut current = root;
    for &idx in path {
        current = step_expr(current, idx)
            .ok_or_else(|| TransformError::UnknownNode { at: target_id.into() })?;
    }
    Ok(current)
}

/// Single step into an expression's `idx`-th child, mirroring the
/// walk order in `ids::walk_expr`. Returns `None` when the index
/// is out of range or when the kind has no addressable children.
fn step_expr(e: &mut CExpr, idx: usize) -> Option<&mut CExpr> {
    match e {
        CExpr::Call { callee, args } => {
            if idx == 0 { return Some(callee); }
            args.get_mut(idx - 1)
        }
        CExpr::Let { value, body, .. } => {
            match idx {
                0 => Some(value),
                1 => Some(body),
                _ => None,
            }
        }
        CExpr::Match { scrutinee, arms } => {
            if idx == 0 { return Some(scrutinee); }
            // Arms are walked as Pattern then Body; only the body
            // is reachable as a CExpr child. ids::walk_expr emits
            // pattern + body for each arm, so arm i's body lives
            // at child index (1 + 2*i + 1).
            let arm_off = idx - 1;
            if arm_off % 2 != 1 {
                return None;
            }
            let arm_index = arm_off / 2;
            arms.get_mut(arm_index).map(|a| &mut a.body)
        }
        CExpr::Block { statements, result } => {
            if idx < statements.len() {
                statements.get_mut(idx)
            } else if idx == statements.len() {
                Some(result)
            } else {
                None
            }
        }
        CExpr::Constructor { args, .. } | CExpr::TupleLit { items: args, .. }
        | CExpr::ListLit { items: args, .. } => args.get_mut(idx),
        CExpr::RecordLit { fields } => fields.get_mut(idx).map(|f| &mut f.value),
        CExpr::FieldAccess { value, .. } => if idx == 0 { Some(value) } else { None },
        CExpr::Lambda { body, .. } => if idx == 0 { Some(body) } else { None },
        CExpr::BinOp { lhs, rhs, .. } => match idx {
            0 => Some(lhs), 1 => Some(rhs), _ => None,
        },
        CExpr::UnaryOp { expr, .. } => if idx == 0 { Some(expr) } else { None },
        CExpr::Return { value } => if idx == 0 { Some(value) } else { None },
        _ => None,
    }
}

fn cexpr_kind(e: &CExpr) -> &'static str {
    match e {
        CExpr::Literal { .. } => "Literal",
        CExpr::Var { .. } => "Var",
        CExpr::Call { .. } => "Call",
        CExpr::Let { .. } => "Let",
        CExpr::Match { .. } => "Match",
        CExpr::Block { .. } => "Block",
        CExpr::Constructor { .. } => "Constructor",
        CExpr::RecordLit { .. } => "RecordLit",
        CExpr::TupleLit { .. } => "TupleLit",
        CExpr::ListLit { .. } => "ListLit",
        CExpr::FieldAccess { .. } => "FieldAccess",
        CExpr::Lambda { .. } => "Lambda",
        CExpr::BinOp { .. } => "BinOp",
        CExpr::UnaryOp { .. } => "UnaryOp",
        CExpr::Return { .. } => "Return",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical::{Arm, CLit, FnDecl, Param, Pattern, TypeExpr};

    // ---- rename_local fixtures + tests -----------------------------

    /// `fn outer(n :: Int) -> Int { let x := n + 1; x + 2 }`
    fn let_stage() -> Stage {
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::BinOp {
                op: "+".into(),
                lhs: Box::new(CExpr::Var { name: "n".into() }),
                rhs: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            }),
            body: Box::new(CExpr::BinOp {
                op: "+".into(),
                lhs: Box::new(CExpr::Var { name: "x".into() }),
                rhs: Box::new(CExpr::Literal { value: CLit::Int { value: 2 } }),
            }),
        };
        Stage::FnDecl(FnDecl {
            name: "outer".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "n".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        })
    }

    fn let_node_id() -> NodeId { NodeId("n_0.2".into()) }

    #[test]
    fn rename_local_renames_binding_and_body_reference() {
        let stage = let_stage();
        let out = rename_local(&stage, &let_node_id(), "y").unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Let { name, value, body, .. } = fd.body else { panic!() };
        assert_eq!(name, "y", "binding renamed");
        // value untouched (no `x` ref in `n + 1`).
        let CExpr::BinOp { lhs, .. } = *value else { panic!() };
        assert!(matches!(*lhs, CExpr::Var { name: ref n } if n == "n"));
        // Body's `x` ref renamed to `y`.
        let CExpr::BinOp { lhs, .. } = *body else { panic!() };
        assert!(matches!(*lhs, CExpr::Var { name: ref n } if n == "y"));
    }

    #[test]
    fn rename_local_refuses_no_op() {
        let stage = let_stage();
        let err = rename_local(&stage, &let_node_id(), "x").unwrap_err();
        assert!(matches!(err, TransformError::RenameNoOp { .. }));
    }

    #[test]
    fn rename_local_respects_inner_let_shadowing() {
        // `fn f() -> Int { let x := 1; let x := 2; x }`
        // Renaming the OUTER `x` to `y` should leave both inner
        // references unchanged because the inner let shadows.
        let inner = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 2 } }),
            body: Box::new(CExpr::Var { name: "x".into() }),
        };
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            body: Box::new(inner),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "f".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        let out = rename_local(&stage, &NodeId("n_0.1".into()), "y").unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Let { name: outer_name, body: outer_body, .. } = fd.body else { panic!() };
        assert_eq!(outer_name, "y", "outer let renamed");
        let CExpr::Let { name: inner_name, body: inner_body, .. } = *outer_body else { panic!() };
        // Inner let shadows — its name stays "x".
        assert_eq!(inner_name, "x");
        // Inner body's `x` is the inner binding, not renamed.
        assert!(matches!(*inner_body, CExpr::Var { name: ref n } if n == "x"));
    }

    #[test]
    fn rename_local_respects_lambda_param_shadowing() {
        // Body: `let x := 1; (\x. x)`. Lambda param shadows.
        let lambda = CExpr::Lambda {
            params: vec![Param {
                name: "x".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            effects: Vec::new(),
            body: Box::new(CExpr::Var { name: "x".into() }),
        };
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            body: Box::new(lambda),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "f".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Function {
                params: vec![TypeExpr::Named { name: "Int".into(), args: Vec::new() }],
                effects: Vec::new(),
                ret: Box::new(TypeExpr::Named { name: "Int".into(), args: Vec::new() }),
            },
            body,
            examples: Vec::new(),
        });
        let out = rename_local(&stage, &NodeId("n_0.1".into()), "y").unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Let { name, body: outer_body, .. } = fd.body else { panic!() };
        assert_eq!(name, "y");
        let CExpr::Lambda { body: lam_body, .. } = *outer_body else { panic!() };
        // Lambda body's `x` is the param, not renamed.
        assert!(matches!(*lam_body, CExpr::Var { name: ref n } if n == "x"));
    }

    #[test]
    fn rename_local_respects_match_pattern_shadowing() {
        // Body: `let x := 1; match foo { x => x, _ => x }`
        // The first arm's pattern is `PVar { x }` — it binds x,
        // shadowing the let; that arm's body's `x` is the pattern
        // binding, not the let binding. Should not be renamed.
        // The `_` wildcard does NOT shadow; its body's `x` is the
        // let binding, should be renamed.
        let match_expr = CExpr::Match {
            scrutinee: Box::new(CExpr::Var { name: "foo".into() }),
            arms: vec![
                Arm {
                    pattern: Pattern::PVar { name: "x".into() },
                    body: CExpr::Var { name: "x".into() },
                },
                Arm {
                    pattern: Pattern::PWild,
                    body: CExpr::Var { name: "x".into() },
                },
            ],
        };
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            body: Box::new(match_expr),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "f".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        let out = rename_local(&stage, &NodeId("n_0.1".into()), "y").unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Let { body: outer_body, .. } = fd.body else { panic!() };
        let CExpr::Match { arms, .. } = *outer_body else { panic!() };
        // Arm 0 pattern binds x; arm body's `x` is the pattern, NOT renamed.
        assert!(matches!(arms[0].body, CExpr::Var { name: ref n } if n == "x"));
        // Arm 1 has wildcard; body's `x` is the let, renamed to `y`.
        assert!(matches!(arms[1].body, CExpr::Var { name: ref n } if n == "y"));
    }

    #[test]
    fn rename_local_not_a_let_errors() {
        // Stage with a plain Match body (no Let). Targeting `n_0.2`
        // should error with NotALet.
        let stage = match_stage_with_two_arms();
        let err = rename_local(&stage, &NodeId("n_0.2".into()), "y").unwrap_err();
        assert!(matches!(err, TransformError::NotALet { found_kind: "Match", .. }),
            "got {err:?}");
    }

    // ---- inline_let fixtures + tests -------------------------------

    /// `fn f(n :: Int) -> Int { let x := 5; x + n }`
    fn inlinable_stage() -> Stage {
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 5 } }),
            body: Box::new(CExpr::BinOp {
                op: "+".into(),
                lhs: Box::new(CExpr::Var { name: "x".into() }),
                rhs: Box::new(CExpr::Var { name: "n".into() }),
            }),
        };
        Stage::FnDecl(FnDecl {
            name: "f".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "n".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        })
    }

    #[test]
    fn inline_let_substitutes_literal_value() {
        let stage = inlinable_stage();
        let out = inline_let(&stage, &NodeId("n_0.2".into())).unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        // The Let is gone; body root is now `5 + n`.
        let CExpr::BinOp { lhs, .. } = fd.body else { panic!() };
        assert!(matches!(*lhs, CExpr::Literal { value: CLit::Int { value: 5 } }));
    }

    #[test]
    fn inline_let_refuses_call_in_value() {
        // `let x := f(); x`  →  refused, `f()` may have effects.
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Call {
                callee: Box::new(CExpr::Var { name: "f".into() }),
                args: Vec::new(),
            }),
            body: Box::new(CExpr::Var { name: "x".into() }),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "g".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        let err = inline_let(&stage, &NodeId("n_0.1".into())).unwrap_err();
        assert!(matches!(err, TransformError::InlineLetRefused { .. }), "got {err:?}");
    }

    #[test]
    fn inline_let_refuses_capture() {
        // `let x := y; let y := 7; x + y`
        // Inlining `x` would capture `y`. Must refuse.
        let inner = CExpr::Let {
            name: "y".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 7 } }),
            body: Box::new(CExpr::BinOp {
                op: "+".into(),
                lhs: Box::new(CExpr::Var { name: "x".into() }),
                rhs: Box::new(CExpr::Var { name: "y".into() }),
            }),
        };
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Var { name: "y".into() }),
            body: Box::new(inner),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "g".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "y".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        // Let `x` is at child index 2 (1 param + return + body slot).
        let err = inline_let(&stage, &NodeId("n_0.2".into())).unwrap_err();
        assert!(matches!(err, TransformError::InlineLetRefused { .. }), "got {err:?}");
    }

    #[test]
    fn inline_let_substitutes_under_shadowing() {
        // `let x := 5; let x := n; x + 1`
        // Inner let SHADOWS the outer `x`. Inlining the OUTER `x`
        // should affect nothing because the inner shadow cuts off
        // descent. The outer let is still eliminated.
        let inner = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Var { name: "n".into() }),
            body: Box::new(CExpr::BinOp {
                op: "+".into(),
                lhs: Box::new(CExpr::Var { name: "x".into() }),
                rhs: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            }),
        };
        let body = CExpr::Let {
            name: "x".into(),
            ty: None,
            value: Box::new(CExpr::Literal { value: CLit::Int { value: 5 } }),
            body: Box::new(inner),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "g".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "n".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        let out = inline_let(&stage, &NodeId("n_0.2".into())).unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        // Outer let removed; body is the inner Let.
        let CExpr::Let { name, .. } = fd.body else { panic!() };
        assert_eq!(name, "x", "inner let preserved");
    }

    #[test]
    fn inline_let_not_a_let_target_errors() {
        let stage = match_stage_with_two_arms();
        let err = inline_let(&stage, &NodeId("n_0.2".into())).unwrap_err();
        assert!(matches!(err, TransformError::NotALet { found_kind: "Match", .. }));
    }

    // ---- extract_function tests ------------------------------------

    /// `fn caller(n :: Int, m :: Int) -> Int { (n * 2) + m }`
    /// We'll extract the `n * 2` sub-expression into a new fn.
    fn extract_stage() -> Stage {
        let body = CExpr::BinOp {
            op: "+".into(),
            lhs: Box::new(CExpr::BinOp {
                op: "*".into(),
                lhs: Box::new(CExpr::Var { name: "n".into() }),
                rhs: Box::new(CExpr::Literal { value: CLit::Int { value: 2 } }),
            }),
            rhs: Box::new(CExpr::Var { name: "m".into() }),
        };
        Stage::FnDecl(FnDecl {
            name: "caller".into(),
            type_params: Vec::new(),
            params: vec![
                Param {
                    name: "n".into(),
                    ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
                },
                Param {
                    name: "m".into(),
                    ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
                },
            ],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        })
    }

    fn double_n_spec() -> ExtractFnSpec {
        ExtractFnSpec {
            name: "double_n".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "n".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        }
    }

    #[test]
    fn extract_function_replaces_subexpression_with_call() {
        // 2 params + return + body slot → body is at child 3.
        // The body is a BinOp; lhs (the `n * 2` sub-expr) is at
        // child 0 of the body. So target NodeId = `n_0.3.0`.
        let stage = extract_stage();
        let (modified, new_fn) = extract_function(
            &stage,
            &NodeId("n_0.3.0".into()),
            double_n_spec(),
        ).unwrap();

        // Modified source: lhs is now `Call { Var(double_n), [Var(n)] }`.
        let Stage::FnDecl(fd) = modified else { panic!() };
        let CExpr::BinOp { lhs, .. } = fd.body else { panic!() };
        let CExpr::Call { callee, args } = *lhs else { panic!() };
        assert!(matches!(*callee, CExpr::Var { name: ref n } if n == "double_n"));
        assert_eq!(args.len(), 1);
        assert!(matches!(args[0], CExpr::Var { name: ref n } if n == "n"));

        // New fn: name + params + body all match.
        let Stage::FnDecl(new_fd) = new_fn else { panic!() };
        assert_eq!(new_fd.name, "double_n");
        assert_eq!(new_fd.params.len(), 1);
        assert_eq!(new_fd.params[0].name, "n");
        // Body is the original `n * 2`.
        let CExpr::BinOp { op, lhs, rhs, .. } = new_fd.body else { panic!() };
        assert_eq!(op, "*");
        assert!(matches!(*lhs, CExpr::Var { name: ref n } if n == "n"));
        assert!(matches!(*rhs, CExpr::Literal { value: CLit::Int { value: 2 } }));
    }

    #[test]
    fn extract_function_refuses_extra_params() {
        // Spec declares an extra param `z` not free in `n * 2`.
        let stage = extract_stage();
        let mut spec = double_n_spec();
        spec.params.push(Param {
            name: "z".into(),
            ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        });
        let err = extract_function(&stage, &NodeId("n_0.3.0".into()), spec).unwrap_err();
        assert!(matches!(err, TransformError::ExtractFnRefused { .. }), "got {err:?}");
    }

    #[test]
    fn extract_function_refuses_missing_params() {
        // Spec omits `n`, which IS free in `n * 2`.
        let stage = extract_stage();
        let spec = ExtractFnSpec {
            name: "no_args".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        };
        let err = extract_function(&stage, &NodeId("n_0.3.0".into()), spec).unwrap_err();
        assert!(matches!(err, TransformError::ExtractFnRefused { .. }), "got {err:?}");
    }

    #[test]
    fn extract_function_handles_zero_free_vars() {
        // Body: `1 + 2`. Extract the LHS literal `1`. No free vars.
        let body = CExpr::BinOp {
            op: "+".into(),
            lhs: Box::new(CExpr::Literal { value: CLit::Int { value: 1 } }),
            rhs: Box::new(CExpr::Literal { value: CLit::Int { value: 2 } }),
        };
        let stage = Stage::FnDecl(FnDecl {
            name: "caller".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        });
        let spec = ExtractFnSpec {
            name: "one".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        };
        // 0 params + return + body = body at child 1; lhs at 1.0.
        let (modified, new_fn) = extract_function(
            &stage, &NodeId("n_0.1.0".into()), spec,
        ).unwrap();
        let Stage::FnDecl(fd) = modified else { panic!() };
        let CExpr::BinOp { lhs, .. } = fd.body else { panic!() };
        let CExpr::Call { args, .. } = *lhs else { panic!() };
        assert_eq!(args.len(), 0, "no args for zero-free-var extract");
        let Stage::FnDecl(new_fd) = new_fn else { panic!() };
        assert!(matches!(new_fd.body, CExpr::Literal { value: CLit::Int { value: 1 } }));
    }

    #[test]
    fn extract_function_typedecl_target_errors() {
        let stage = Stage::TypeDecl(crate::canonical::TypeDecl {
            name: "T".into(),
            params: Vec::new(),
            definition: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        });
        let err = extract_function(&stage, &NodeId("n_0.0".into()), double_n_spec())
            .unwrap_err();
        assert!(matches!(err, TransformError::NonFnTarget { stage_kind: "TypeDecl" }));
    }

    // ---- replace_match_arm fixtures + tests ------------------------

    fn match_stage_with_two_arms() -> Stage {
        // fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }
        // Two arms: pattern-literal-0 → 1; wildcard → 2.
        let body = CExpr::Match {
            scrutinee: Box::new(CExpr::Var { name: "n".into() }),
            arms: vec![
                Arm {
                    pattern: Pattern::PLiteral { value: CLit::Int { value: 0 } },
                    body: CExpr::Literal { value: CLit::Int { value: 1 } },
                },
                Arm {
                    pattern: Pattern::PWild,
                    body: CExpr::Literal { value: CLit::Int { value: 2 } },
                },
            ],
        };
        Stage::FnDecl(FnDecl {
            name: "pick".into(),
            type_params: Vec::new(),
            params: vec![Param {
                name: "n".into(),
                ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            }],
            effects: Vec::new(),
            return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
            body,
            examples: Vec::new(),
        })
    }

    fn match_node_id() -> NodeId {
        // FnDecl with 1 param: body is at child index 2 (param 0,
        // return_type at 1, body at 2). The body IS the Match
        // expression — so the match node is at `n_0.2`.
        NodeId("n_0.2".into())
    }

    #[test]
    fn replace_first_arm_body_succeeds() {
        let stage = match_stage_with_two_arms();
        let new_body = CExpr::Literal { value: CLit::Int { value: 42 } };
        let out = replace_match_arm(&stage, &match_node_id(), 0, new_body).unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Match { arms, .. } = fd.body else { panic!() };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[0].body, CExpr::Literal { value: CLit::Int { value: 42 } }));
        // Second arm untouched.
        assert!(matches!(arms[1].body, CExpr::Literal { value: CLit::Int { value: 2 } }));
        // Pattern preserved.
        assert!(matches!(arms[0].pattern, Pattern::PLiteral { .. }));
    }

    #[test]
    fn replace_second_arm_preserves_first() {
        let stage = match_stage_with_two_arms();
        let new_body = CExpr::Literal { value: CLit::Int { value: 99 } };
        let out = replace_match_arm(&stage, &match_node_id(), 1, new_body).unwrap();
        let Stage::FnDecl(fd) = out else { panic!() };
        let CExpr::Match { arms, .. } = fd.body else { panic!() };
        assert!(matches!(arms[0].body, CExpr::Literal { value: CLit::Int { value: 1 } }));
        assert!(matches!(arms[1].body, CExpr::Literal { value: CLit::Int { value: 99 } }));
    }

    #[test]
    fn arm_index_out_of_range_errors() {
        let stage = match_stage_with_two_arms();
        let new_body = CExpr::Literal { value: CLit::Unit };
        let err = replace_match_arm(&stage, &match_node_id(), 5, new_body).unwrap_err();
        assert!(matches!(err, TransformError::ArmIndexOutOfRange { arm_count: 2, requested: 5, .. }));
    }

    #[test]
    fn non_match_target_errors() {
        // The body's scrutinee at n_0.2.0 is a Var, not a Match.
        let stage = match_stage_with_two_arms();
        let new_body = CExpr::Literal { value: CLit::Unit };
        let err = replace_match_arm(&stage, &NodeId("n_0.2.0".into()), 0, new_body)
            .unwrap_err();
        assert!(matches!(err, TransformError::NotAMatch { found_kind: "Var", .. }),
            "got {err:?}");
    }

    #[test]
    fn unknown_node_errors() {
        let stage = match_stage_with_two_arms();
        let new_body = CExpr::Literal { value: CLit::Unit };
        let err = replace_match_arm(&stage, &NodeId("n_0.99".into()), 0, new_body)
            .unwrap_err();
        assert!(matches!(err, TransformError::UnknownNode { .. }), "got {err:?}");
    }

    #[test]
    fn typedecl_target_errors() {
        let stage = Stage::TypeDecl(crate::canonical::TypeDecl {
            name: "T".into(),
            params: Vec::new(),
            definition: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        });
        let err = replace_match_arm(&stage, &match_node_id(), 0,
            CExpr::Literal { value: CLit::Unit }).unwrap_err();
        assert!(matches!(err, TransformError::NonFnTarget { stage_kind: "TypeDecl" }));
    }
}
