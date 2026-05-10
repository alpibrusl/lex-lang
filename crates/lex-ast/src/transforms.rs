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
//! # Slice 1 (#280)
//!
//! [`replace_match_arm`] — replaces the body of one arm in a match
//! expression. Pattern is preserved; the new body must be a valid
//! `CExpr`. This is the simplest position-targeted transform and
//! establishes the pattern for the rest (`RenameLocal`,
//! `InlineLet`, `ExtractFunction` — separate slices).

use crate::canonical::{CExpr, Stage};
use crate::ids::NodeId;

#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransformError {
    #[error("unknown node id `{at}`")]
    UnknownNode { at: String },
    #[error("expected a Match expression at `{at}` but found `{found_kind}`")]
    NotAMatch { at: String, found_kind: &'static str },
    #[error("arm index {requested} out of range (arm count = {arm_count}) at `{at}`")]
    ArmIndexOutOfRange { at: String, arm_count: usize, requested: usize },
    #[error("malformed NodeId `{0}`")]
    BadNodeId(String),
    #[error("cannot transform inside `{stage_kind}` — only FnDecl bodies are transformable")]
    NonFnTarget { stage_kind: &'static str },
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
