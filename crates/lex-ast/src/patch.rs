//! Canonical-AST patches per spec §5.4.
//!
//! A patch is a structured edit to a `Stage`. Operations are addressed by
//! `NodeId` (the path-based ID from `crate::ids`). Patches are applied
//! transactionally by `apply_patch`: the result is returned as a fresh
//! `Stage` value, leaving the input untouched. The caller is responsible
//! for type-checking and re-publishing the result.

use crate::canonical::{CExpr, Stage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Patch {
    /// Replace the node at `target` with the given expression fragment.
    /// Only `CExpr` nodes are supported (typed positions like type
    /// expressions and patterns are out-of-scope for this op).
    Replace {
        target: String,
        with: CExpr,
    },
    /// Delete a CExpr from a list-shaped parent. Currently supports
    /// `Block.statements[i]`. Deleting the result expression of a Block
    /// or any non-list parent is rejected.
    Delete {
        target: String,
    },
    /// Wrap the target expression with `wrapper`. The wrapper must
    /// contain exactly one `Var { name: "_HOLE_" }` node, which is
    /// substituted by the original target.
    WrapWith {
        target: String,
        wrapper: CExpr,
    },
}

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatchError {
    #[error("unknown node id `{at}`")]
    UnknownNode { at: String },
    #[error("cannot patch non-expression at `{at}` ({reason})")]
    NonExprTarget { at: String, reason: String },
    #[error("Delete target is not in a list-shaped parent at `{at}`")]
    DeleteNotInList { at: String },
    #[error("WrapWith fragment must contain exactly one `_HOLE_` var")]
    WrapWithMissingHole,
    #[error("malformed NodeId `{0}`")]
    BadNodeId(String),
}

/// Apply a patch and return the resulting stage. The input is cloned;
/// the original is untouched.
pub fn apply_patch(stage: &Stage, patch: &Patch) -> Result<Stage, PatchError> {
    let mut out = stage.clone();
    let target = match patch {
        Patch::Replace { target, .. } | Patch::Delete { target } | Patch::WrapWith { target, .. } => target,
    };
    let path = parse_node_id(target)?;

    let body = match &mut out {
        Stage::FnDecl(fd) => &mut fd.body,
        Stage::TypeDecl(_) | Stage::Import(_) => {
            return Err(PatchError::NonExprTarget {
                at: target.clone(),
                reason: "only fn-decl bodies are patchable".into(),
            });
        }
    };

    // The fn body lives at child index = params.len() + 1 of the stage
    // root (params occupy 0..n_params, return type is at n_params, body
    // is at n_params+1). Path[0] is the position relative to the stage
    // root; for the body we expect path[0] == n_params+1, but to keep
    // patches focused on the body we just walk into the body directly:
    // path[1..] addresses inside it.
    if path.is_empty() {
        // The whole stage's "body root" — treat path == [n_params+1].
        return Err(PatchError::NonExprTarget {
            at: target.clone(),
            reason: "cannot replace the stage root".into(),
        });
    }
    // Skip the head index (the position of the body within the stage)
    // — we assume the user is pointing inside the body.
    let body_path = &path[1..];

    apply_inside_expr(body, body_path, patch, target)?;
    Ok(out)
}

/// Parse `n_0.3.1` → `[0, 3, 1]`. Empty after `n_0` is allowed.
fn parse_node_id(id: &str) -> Result<Vec<usize>, PatchError> {
    let s = id.strip_prefix("n_").ok_or_else(|| PatchError::BadNodeId(id.into()))?;
    let mut parts = s.split('.');
    let head = parts.next().ok_or_else(|| PatchError::BadNodeId(id.into()))?;
    if head != "0" {
        return Err(PatchError::BadNodeId(id.into()));
    }
    let mut out = Vec::new();
    for p in parts {
        out.push(p.parse::<usize>().map_err(|_| PatchError::BadNodeId(id.into()))?);
    }
    Ok(out)
}

fn apply_inside_expr(
    e: &mut CExpr,
    path: &[usize],
    patch: &Patch,
    target_id: &str,
) -> Result<(), PatchError> {
    if path.is_empty() {
        // We're at the target.
        match patch {
            Patch::Replace { with, .. } => {
                *e = with.clone();
                Ok(())
            }
            Patch::Delete { .. } => Err(PatchError::DeleteNotInList { at: target_id.into() }),
            Patch::WrapWith { wrapper, .. } => {
                let mut wrapped = wrapper.clone();
                if !substitute_hole(&mut wrapped, e) {
                    return Err(PatchError::WrapWithMissingHole);
                }
                *e = wrapped;
                Ok(())
            }
        }
    } else {
        // Step into the i'th child.
        let i = path[0];
        let rest = &path[1..];
        descend(e, i, rest, patch, target_id)
    }
}

fn descend(
    e: &mut CExpr,
    i: usize,
    rest: &[usize],
    patch: &Patch,
    target_id: &str,
) -> Result<(), PatchError> {
    let unknown = || PatchError::UnknownNode { at: target_id.into() };
    match e {
        CExpr::Call { callee, args } => {
            if i == 0 { return apply_inside_expr(callee, rest, patch, target_id); }
            let idx = i - 1;
            args.get_mut(idx).ok_or_else(unknown)
                .and_then(|c| apply_inside_expr(c, rest, patch, target_id))
        }
        CExpr::Let { value, body, .. } => match i {
            0 => apply_inside_expr(value, rest, patch, target_id),
            1 => apply_inside_expr(body, rest, patch, target_id),
            _ => Err(unknown()),
        },
        CExpr::Match { scrutinee, arms } => {
            if i == 0 { return apply_inside_expr(scrutinee, rest, patch, target_id); }
            // Each arm contributes 2 child slots: pattern (i odd), body (i even, after scrutinee).
            // Layout per `crate::ids::walk_expr`: [scrutinee, arm0_pat, arm0_body, arm1_pat, arm1_body, ...].
            let arm_pos = i - 1;
            let arm_idx = arm_pos / 2;
            let is_pat = arm_pos.is_multiple_of(2);
            let arm = arms.get_mut(arm_idx).ok_or_else(unknown)?;
            if is_pat {
                Err(PatchError::NonExprTarget {
                    at: target_id.into(),
                    reason: "patches on patterns are not supported (use Replace on the arm body or WrapWith)".into(),
                })
            } else {
                apply_inside_expr(&mut arm.body, rest, patch, target_id)
            }
        }
        CExpr::Block { statements, result } => {
            // Layout: statements first, then result.
            if i < statements.len() {
                if rest.is_empty() {
                    // Direct hit on a statement.
                    match patch {
                        Patch::Delete { .. } => {
                            statements.remove(i);
                            Ok(())
                        }
                        Patch::Replace { with, .. } => {
                            statements[i] = with.clone();
                            Ok(())
                        }
                        Patch::WrapWith { wrapper, .. } => {
                            let mut wrapped = wrapper.clone();
                            let original = std::mem::replace(&mut statements[i], CExpr::Literal { value: crate::canonical::CLit::Unit });
                            if !substitute_hole(&mut wrapped, &original) {
                                statements[i] = original; // restore
                                return Err(PatchError::WrapWithMissingHole);
                            }
                            statements[i] = wrapped;
                            Ok(())
                        }
                    }
                } else {
                    apply_inside_expr(&mut statements[i], rest, patch, target_id)
                }
            } else if i == statements.len() {
                // The result expression. Delete is meaningless here.
                if matches!(patch, Patch::Delete { .. }) {
                    Err(PatchError::DeleteNotInList { at: target_id.into() })
                } else {
                    apply_inside_expr(result, rest, patch, target_id)
                }
            } else {
                Err(unknown())
            }
        }
        CExpr::Constructor { args, .. } => {
            args.get_mut(i).ok_or_else(unknown)
                .and_then(|c| apply_inside_expr(c, rest, patch, target_id))
        }
        CExpr::RecordLit { fields } => {
            fields.get_mut(i).ok_or_else(unknown)
                .and_then(|f| apply_inside_expr(&mut f.value, rest, patch, target_id))
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            if rest.is_empty() && matches!(patch, Patch::Delete { .. }) {
                if i >= items.len() { return Err(unknown()); }
                items.remove(i);
                return Ok(());
            }
            items.get_mut(i).ok_or_else(unknown)
                .and_then(|c| apply_inside_expr(c, rest, patch, target_id))
        }
        CExpr::FieldAccess { value, .. } => {
            if i == 0 { apply_inside_expr(value, rest, patch, target_id) } else { Err(unknown()) }
        }
        CExpr::Lambda { body, .. } => {
            if i == 0 { apply_inside_expr(body, rest, patch, target_id) } else { Err(unknown()) }
        }
        CExpr::BinOp { lhs, rhs, .. } => match i {
            0 => apply_inside_expr(lhs, rest, patch, target_id),
            1 => apply_inside_expr(rhs, rest, patch, target_id),
            _ => Err(unknown()),
        },
        CExpr::UnaryOp { expr, .. } => {
            if i == 0 { apply_inside_expr(expr, rest, patch, target_id) } else { Err(unknown()) }
        }
        CExpr::Return { value } => {
            if i == 0 { apply_inside_expr(value, rest, patch, target_id) } else { Err(unknown()) }
        }
        CExpr::Literal { .. } | CExpr::Var { .. } => Err(unknown()),
    }
}

/// Find the unique `Var { name: "_HOLE_" }` and replace it with `target`.
/// Returns false if no hole was found.
fn substitute_hole(node: &mut CExpr, target: &CExpr) -> bool {
    let mut count = 0;
    walk_substitute(node, target, &mut count);
    count == 1
}

fn walk_substitute(e: &mut CExpr, target: &CExpr, count: &mut u32) {
    if let CExpr::Var { name } = e {
        if name == "_HOLE_" {
            *e = target.clone();
            *count += 1;
            return;
        }
    }
    match e {
        CExpr::Literal { .. } | CExpr::Var { .. } => {}
        CExpr::Call { callee, args } => {
            walk_substitute(callee, target, count);
            for a in args { walk_substitute(a, target, count); }
        }
        CExpr::Let { value, body, .. } => {
            walk_substitute(value, target, count);
            walk_substitute(body, target, count);
        }
        CExpr::Match { scrutinee, arms } => {
            walk_substitute(scrutinee, target, count);
            for a in arms { walk_substitute(&mut a.body, target, count); }
        }
        CExpr::Block { statements, result } => {
            for s in statements { walk_substitute(s, target, count); }
            walk_substitute(result, target, count);
        }
        CExpr::Constructor { args, .. } => for a in args { walk_substitute(a, target, count); },
        CExpr::RecordLit { fields } => for f in fields { walk_substitute(&mut f.value, target, count); },
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items { walk_substitute(i, target, count); }
        }
        CExpr::FieldAccess { value, .. } => walk_substitute(value, target, count),
        CExpr::Lambda { body, .. } => walk_substitute(body, target, count),
        CExpr::BinOp { lhs, rhs, .. } => {
            walk_substitute(lhs, target, count);
            walk_substitute(rhs, target, count);
        }
        CExpr::UnaryOp { expr, .. } => walk_substitute(expr, target, count),
        CExpr::Return { value } => walk_substitute(value, target, count),
    }
}
