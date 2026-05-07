//! AST-level structural diff between two sets of `FnDecl`s.
//!
//! Moved from `lex-cli/src/diff.rs` so both the CLI (`lex diff`
//! command) and the HTTP API (`lex serve`) can compute a [`DiffReport`]
//! without introducing a circular dependency. `lex-vcs` is the right
//! home because it already owns `DiffReport` and `diff_to_ops`.

use crate::diff_report::{
    AddRemove, BodyPatch, DiffReport, EffectChanges, Modified, Renamed,
};
use lex_ast::{stage_canonical_hash_hex, CExpr, Effect, EffectArg, FnDecl, Stage, TypeExpr};
use std::collections::{BTreeMap, BTreeSet};

/// Compute a structural diff between two named fn-decl maps.
///
/// `body_patches` controls whether body-level expression diffs are
/// emitted inside each `Modified` entry. Pass `true` for rich output
/// (CLI / review); `false` for a signature-only diff (faster).
pub fn compute_diff(
    a: &BTreeMap<String, FnDecl>,
    b: &BTreeMap<String, FnDecl>,
    body_patches: bool,
) -> DiffReport {
    let mut report = DiffReport::default();
    let names_a: BTreeSet<&String> = a.keys().collect();
    let names_b: BTreeSet<&String> = b.keys().collect();

    let only_a: Vec<&String> = names_a.difference(&names_b).copied().collect();
    let only_b: Vec<&String> = names_b.difference(&names_a).copied().collect();

    // Detect renames: for each name only-in-A, check if any only-in-B
    // has a body whose canonical-AST hash matches (modulo the fn
    // name itself). sig_id over the FnDecl with name normalized
    // serves as the structural-identity key.
    let mut renamed_pairs: Vec<(String, String)> = Vec::new();
    let mut consumed_a: BTreeSet<String> = BTreeSet::new();
    let mut consumed_b: BTreeSet<String> = BTreeSet::new();
    for &an in &only_a {
        let fa = &a[an];
        let fa_norm_id = body_hash(fa);
        for &bn in &only_b {
            if consumed_b.contains(bn) { continue; }
            let fb = &b[bn];
            if body_hash(fb) == fa_norm_id {
                renamed_pairs.push((an.clone(), bn.clone()));
                consumed_a.insert(an.clone());
                consumed_b.insert(bn.clone());
                break;
            }
        }
    }

    for &n in &only_a {
        if consumed_a.contains(n) { continue; }
        let fd = &a[n];
        report.removed.push(AddRemove {
            name: n.clone(),
            signature: render_signature(fd),
        });
    }
    for &n in &only_b {
        if consumed_b.contains(n) { continue; }
        let fd = &b[n];
        report.added.push(AddRemove {
            name: n.clone(),
            signature: render_signature(fd),
        });
    }
    for (an, bn) in &renamed_pairs {
        let fd = &b[bn];
        report.renamed.push(Renamed {
            from: an.clone(),
            to: bn.clone(),
            signature: render_signature(fd),
        });
    }

    // Modified: same name on both sides; compare bodies.
    for n in names_a.intersection(&names_b) {
        let fa = &a[*n];
        let fb = &b[*n];
        let sig_a = render_signature(fa);
        let sig_b = render_signature(fb);
        if body_hash(fa) == body_hash(fb) && sig_a == sig_b { continue; }

        let patches = if body_patches {
            let mut patches = Vec::new();
            diff_expr(&fa.body, &fb.body, "body", &mut patches, 4);
            patches
        } else { Vec::new() };

        let effect_changes = effect_diff(&fa.effects, &fb.effects);
        report.modified.push(Modified {
            name: (*n).clone(),
            signature_before: sig_a.clone(),
            signature_after: sig_b.clone(),
            signature_changed: sig_a != sig_b,
            effect_changes,
            body_patches: patches,
        });
    }
    report
}

/// Hash of the function's structural identity, used for rename
/// detection. Excludes the function's name (so `fn foo -> Int { 1 }`
/// and `fn bar -> Int { 1 }` share a hash) but includes everything
/// else: params, effects, return type, body.
fn body_hash(fd: &FnDecl) -> String {
    let mut anon = fd.clone();
    anon.name = String::new();
    let stage = Stage::FnDecl(anon);
    stage_canonical_hash_hex(&stage)
}

/// Walk two CExprs in parallel; record the first divergence at each
/// child position. `depth` caps recursion so a tiny per-fn diff
/// doesn't degenerate into hundreds of micro-changes.
fn diff_expr(a: &CExpr, b: &CExpr, path: &str, out: &mut Vec<BodyPatch>, depth: u32) {
    if depth == 0 { return; }
    let kind_a = node_kind(a);
    let kind_b = node_kind(b);
    if kind_a != kind_b {
        out.push(BodyPatch {
            op: "Replace".into(), node_path: path.into(),
            from_kind: kind_a.into(), to_kind: kind_b.into(),
        });
        return;
    }
    // Same kind: recurse into structurally-equivalent children.
    match (a, b) {
        (CExpr::Literal { value: la }, CExpr::Literal { value: lb }) => {
            if la != lb {
                out.push(BodyPatch {
                    op: "Replace".into(), node_path: path.into(),
                    from_kind: "Literal".into(), to_kind: "Literal".into(),
                });
            }
        }
        (CExpr::Var { name: na }, CExpr::Var { name: nb }) => {
            if na != nb {
                out.push(BodyPatch {
                    op: "Replace".into(), node_path: path.into(),
                    from_kind: format!("Var({na})"), to_kind: format!("Var({nb})"),
                });
            }
        }
        (CExpr::Call { callee: ca, args: aa },
         CExpr::Call { callee: cb, args: ab }) => {
            diff_expr(ca, cb, &format!("{path}.callee"), out, depth - 1);
            diff_args(aa, ab, &format!("{path}.args"), out, depth);
        }
        (CExpr::Let { name: na, value: va, body: ba, .. },
         CExpr::Let { name: nb, value: vb, body: bb, .. }) => {
            if na != nb {
                out.push(BodyPatch {
                    op: "Replace".into(),
                    node_path: format!("{path}.name"),
                    from_kind: format!("Let({na})"),
                    to_kind:   format!("Let({nb})"),
                });
            }
            diff_expr(va, vb, &format!("{path}.value"), out, depth - 1);
            diff_expr(ba, bb, &format!("{path}.body"),  out, depth - 1);
        }
        (CExpr::Match { scrutinee: sa, arms: ams },
         CExpr::Match { scrutinee: sb, arms: bms }) => {
            diff_expr(sa, sb, &format!("{path}.scrutinee"), out, depth - 1);
            let n = ams.len().max(bms.len());
            for i in 0..n {
                let p = format!("{path}.arms[{i}]");
                match (ams.get(i), bms.get(i)) {
                    (Some(a), Some(b)) =>
                        diff_expr(&a.body, &b.body, &p, out, depth - 1),
                    (Some(_), None) => out.push(BodyPatch {
                        op: "Deleted".into(), node_path: p,
                        from_kind: "MatchArm".into(), to_kind: "(removed)".into(),
                    }),
                    (None, Some(_)) => out.push(BodyPatch {
                        op: "Inserted".into(), node_path: p,
                        from_kind: "(none)".into(), to_kind: "MatchArm".into(),
                    }),
                    (None, None) => break,
                }
            }
        }
        (CExpr::Block { statements: sa, result: ra },
         CExpr::Block { statements: sb, result: rb }) => {
            diff_args(sa, sb, &format!("{path}.statements"), out, depth);
            diff_expr(ra, rb, &format!("{path}.result"), out, depth - 1);
        }
        (CExpr::FieldAccess { value: va, field: fa },
         CExpr::FieldAccess { value: vb, field: fb }) => {
            diff_expr(va, vb, &format!("{path}.value"), out, depth - 1);
            if fa != fb {
                out.push(BodyPatch {
                    op: "Replace".into(), node_path: format!("{path}.field"),
                    from_kind: format!("Field({fa})"), to_kind: format!("Field({fb})"),
                });
            }
        }
        (CExpr::Lambda { body: ba, .. }, CExpr::Lambda { body: bb, .. }) => {
            diff_expr(ba, bb, &format!("{path}.body"), out, depth - 1);
        }
        // For shapes we don't unfold further, mark the node itself
        // as edited (same kind, content differs) — finer detail can
        // come in a follow-up.
        _ => {
            out.push(BodyPatch {
                op: "Replace".into(), node_path: path.into(),
                from_kind: kind_a.into(), to_kind: kind_b.into(),
            });
        }
    }
}

fn diff_args(a: &[CExpr], b: &[CExpr], path: &str, out: &mut Vec<BodyPatch>, depth: u32) {
    let n = a.len().max(b.len());
    for i in 0..n {
        let p = format!("{path}[{i}]");
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) => diff_expr(x, y, &p, out, depth - 1),
            (Some(x), None) => out.push(BodyPatch {
                op: "Deleted".into(), node_path: p,
                from_kind: node_kind(x).into(), to_kind: "(removed)".into(),
            }),
            (None, Some(y)) => out.push(BodyPatch {
                op: "Inserted".into(), node_path: p,
                from_kind: "(none)".into(), to_kind: node_kind(y).into(),
            }),
            (None, None) => break,
        }
    }
}

fn node_kind(e: &CExpr) -> &'static str {
    match e {
        CExpr::Literal { .. }     => "Literal",
        CExpr::Var { .. }         => "Var",
        CExpr::Call { .. }        => "Call",
        CExpr::Let { .. }         => "Let",
        CExpr::Match { .. }       => "Match",
        CExpr::Block { .. }       => "Block",
        CExpr::Constructor { .. } => "Constructor",
        CExpr::RecordLit { .. }   => "RecordLit",
        CExpr::TupleLit { .. }    => "TupleLit",
        CExpr::ListLit { .. }     => "ListLit",
        CExpr::FieldAccess { .. } => "FieldAccess",
        CExpr::Lambda { .. }      => "Lambda",
        CExpr::BinOp { .. }       => "BinOp",
        CExpr::UnaryOp { .. }     => "UnaryOp",
        CExpr::Return { .. }      => "Return",
    }
}

pub fn render_signature(fd: &FnDecl) -> String {
    let params: Vec<String> = fd.params.iter()
        .map(|p| format!("{} :: {}", p.name, render_type(&p.ty))).collect();
    let eff = if fd.effects.is_empty() { String::new() } else {
        let labels: Vec<String> = fd.effects.iter().map(effect_label).collect();
        format!("[{}] ", labels.join(", "))
    };
    format!("fn {}({}) -> {}{}", fd.name, params.join(", "),
        eff, render_type(&fd.return_type))
}

/// Render an effect with its arg if present: `fs_read("/tmp")`,
/// `net("api.example.com")`, or just `io`. Used by both signature
/// rendering and effect-diff so the same string identifies the
/// same effect in either context.
pub fn effect_label(e: &Effect) -> String {
    match &e.arg {
        Some(EffectArg::Str { value })   => format!("{}({:?})", e.name, value),
        Some(EffectArg::Int { value })   => format!("{}({})",   e.name, value),
        Some(EffectArg::Ident { value }) => format!("{}({})",   e.name, value),
        None => e.name.clone(),
    }
}

/// Set-style diff over two effect lists. Order-insensitive within
/// the lists; ordering of the output is sorted-by-label so the
/// JSON is stable.
fn effect_diff(a: &[Effect], b: &[Effect]) -> EffectChanges {
    let labels_a: BTreeSet<String> = a.iter().map(effect_label).collect();
    let labels_b: BTreeSet<String> = b.iter().map(effect_label).collect();
    let added:   Vec<String> = labels_b.difference(&labels_a).cloned().collect();
    let removed: Vec<String> = labels_a.difference(&labels_b).cloned().collect();
    EffectChanges {
        before:  labels_a.into_iter().collect(),
        after:   labels_b.into_iter().collect(),
        added,
        removed,
    }
}

fn render_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named { name, args } => {
            if args.is_empty() { name.clone() }
            else {
                let parts: Vec<String> = args.iter().map(render_type).collect();
                format!("{name}[{}]", parts.join(", "))
            }
        }
        TypeExpr::Tuple { items } => {
            let parts: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", parts.join(", "))
        }
        TypeExpr::Record { fields } => {
            let parts: Vec<String> = fields.iter()
                .map(|f| format!("{} :: {}", f.name, render_type(&f.ty))).collect();
            format!("{{ {} }}", parts.join(", "))
        }
        TypeExpr::Function { params, effects, ret } => {
            let parts: Vec<String> = params.iter().map(render_type).collect();
            let eff = if effects.is_empty() { String::new() } else {
                let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
                format!("[{}] ", names.join(", "))
            };
            format!("({}) -> {}{}", parts.join(", "), eff, render_type(ret))
        }
        TypeExpr::Union { variants } => variants.iter().map(|v| match &v.payload {
            Some(p) => format!("{}({})", v.name, render_type(p)),
            None => v.name.clone(),
        }).collect::<Vec<_>>().join(" | "),
        TypeExpr::Refined { base, binding, .. } => {
            // Render compactly: `Base{x | …}`. The full predicate is
            // captured in the canonical AST and contributes to
            // OpId hashing via lex-vcs's content-addressing — this
            // string is for diagnostics only. (#209 slice 1)
            format!("{}{{{} | …}}", render_type(base), binding)
        }
    }
}
