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
//!
//! Future slices: `inline_let`, `extract_function`.

use crate::canonical::{CExpr, Pattern, Stage};
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
