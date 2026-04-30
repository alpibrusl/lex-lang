//! Node IDs (§5.2). A NodeId encodes the path from a stage root to a node
//! as `n_0[.<i>]*`, where each `<i>` is the position in the parent's
//! children array.

use crate::canonical::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn root() -> Self { NodeId("n_0".into()) }
    pub fn child(&self, i: usize) -> Self { NodeId(format!("{}.{}", self.0, i)) }
    pub fn as_str(&self) -> &str { &self.0 }
}

/// Walk a stage and emit all NodeIds with their underlying nodes (referenced).
/// Order is depth-first, child-index ordered.
pub fn collect_ids(stage: &Stage) -> Vec<(NodeId, NodeRef<'_>)> {
    let mut out = Vec::new();
    let root = NodeId::root();
    out.push((root.clone(), NodeRef::Stage(stage)));
    walk_stage(stage, &root, &mut out);
    out
}

#[derive(Debug)]
pub enum NodeRef<'a> {
    Stage(&'a Stage),
    CExpr(&'a CExpr),
    Pattern(&'a Pattern),
    TypeExpr(&'a TypeExpr),
}

fn walk_stage<'a>(s: &'a Stage, parent: &NodeId, out: &mut Vec<(NodeId, NodeRef<'a>)>) {
    match s {
        Stage::FnDecl(fd) => {
            // children: params (0..n_params), return_type (n), body (n+1)
            for (i, p) in fd.params.iter().enumerate() {
                let id = parent.child(i);
                out.push((id.clone(), NodeRef::TypeExpr(&p.ty)));
                walk_type(&p.ty, &id, out);
            }
            let rid = parent.child(fd.params.len());
            out.push((rid.clone(), NodeRef::TypeExpr(&fd.return_type)));
            walk_type(&fd.return_type, &rid, out);
            let bid = parent.child(fd.params.len() + 1);
            out.push((bid.clone(), NodeRef::CExpr(&fd.body)));
            walk_expr(&fd.body, &bid, out);
        }
        Stage::TypeDecl(td) => {
            let id = parent.child(0);
            out.push((id.clone(), NodeRef::TypeExpr(&td.definition)));
            walk_type(&td.definition, &id, out);
        }
        Stage::Import(_) => {}
    }
}

fn walk_expr<'a>(e: &'a CExpr, parent: &NodeId, out: &mut Vec<(NodeId, NodeRef<'a>)>) {
    let mut idx = 0;
    let emit_expr = |child: &'a CExpr, idx: &mut usize, out: &mut Vec<(NodeId, NodeRef<'a>)>| {
        let id = parent.child(*idx);
        out.push((id.clone(), NodeRef::CExpr(child)));
        walk_expr(child, &id, out);
        *idx += 1;
    };
    let emit_pat = |p: &'a Pattern, idx: &mut usize, out: &mut Vec<(NodeId, NodeRef<'a>)>| {
        let id = parent.child(*idx);
        out.push((id.clone(), NodeRef::Pattern(p)));
        walk_pat(p, &id, out);
        *idx += 1;
    };
    match e {
        CExpr::Literal { .. } | CExpr::Var { .. } => {}
        CExpr::Call { callee, args } => {
            emit_expr(callee, &mut idx, out);
            for a in args { emit_expr(a, &mut idx, out); }
        }
        CExpr::Let { value, body, .. } => {
            emit_expr(value, &mut idx, out);
            emit_expr(body, &mut idx, out);
        }
        CExpr::Match { scrutinee, arms } => {
            emit_expr(scrutinee, &mut idx, out);
            for arm in arms {
                emit_pat(&arm.pattern, &mut idx, out);
                emit_expr(&arm.body, &mut idx, out);
            }
        }
        CExpr::Block { statements, result } => {
            for s in statements { emit_expr(s, &mut idx, out); }
            emit_expr(result, &mut idx, out);
        }
        CExpr::Constructor { args, .. } => {
            for a in args { emit_expr(a, &mut idx, out); }
        }
        CExpr::RecordLit { fields } => {
            for f in fields { emit_expr(&f.value, &mut idx, out); }
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for it in items { emit_expr(it, &mut idx, out); }
        }
        CExpr::FieldAccess { value, .. } => {
            emit_expr(value, &mut idx, out);
        }
        CExpr::Lambda { body, .. } => {
            emit_expr(body, &mut idx, out);
        }
        CExpr::BinOp { lhs, rhs, .. } => {
            emit_expr(lhs, &mut idx, out);
            emit_expr(rhs, &mut idx, out);
        }
        CExpr::UnaryOp { expr, .. } => {
            emit_expr(expr, &mut idx, out);
        }
        CExpr::Return { value } => {
            emit_expr(value, &mut idx, out);
        }
    }
}

fn walk_pat<'a>(p: &'a Pattern, parent: &NodeId, out: &mut Vec<(NodeId, NodeRef<'a>)>) {
    let mut idx = 0;
    match p {
        Pattern::PLiteral { .. } | Pattern::PVar { .. } | Pattern::PWild => {}
        Pattern::PConstructor { args, .. } => {
            for a in args {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::Pattern(a)));
                walk_pat(a, &id, out);
                idx += 1;
            }
        }
        Pattern::PRecord { fields } => {
            for f in fields {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::Pattern(&f.pattern)));
                walk_pat(&f.pattern, &id, out);
                idx += 1;
            }
        }
        Pattern::PTuple { items } => {
            for it in items {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::Pattern(it)));
                walk_pat(it, &id, out);
                idx += 1;
            }
        }
    }
}

fn walk_type<'a>(t: &'a TypeExpr, parent: &NodeId, out: &mut Vec<(NodeId, NodeRef<'a>)>) {
    let mut idx = 0;
    match t {
        TypeExpr::Named { args, .. } => {
            for a in args {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::TypeExpr(a)));
                walk_type(a, &id, out);
                idx += 1;
            }
        }
        TypeExpr::Record { fields } => {
            for f in fields {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::TypeExpr(&f.ty)));
                walk_type(&f.ty, &id, out);
                idx += 1;
            }
        }
        TypeExpr::Tuple { items } => {
            for it in items {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::TypeExpr(it)));
                walk_type(it, &id, out);
                idx += 1;
            }
        }
        TypeExpr::Function { params, ret, .. } => {
            for p in params {
                let id = parent.child(idx);
                out.push((id.clone(), NodeRef::TypeExpr(p)));
                walk_type(p, &id, out);
                idx += 1;
            }
            let id = parent.child(idx);
            out.push((id.clone(), NodeRef::TypeExpr(ret)));
            walk_type(ret, &id, out);
        }
        TypeExpr::Union { variants } => {
            for v in variants {
                if let Some(p) = &v.payload {
                    let id = parent.child(idx);
                    out.push((id.clone(), NodeRef::TypeExpr(p)));
                    walk_type(p, &id, out);
                }
                idx += 1;
            }
        }
    }
}
