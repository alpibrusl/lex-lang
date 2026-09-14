//! Typed three-way merge over the canonical AST (#838, Tier 4).
//!
//! Whole-function merge (`crate::merge`) treats two divergent bodies
//! for one sig as an indivisible `ModifyModify` conflict. But two
//! agents editing *different* subtrees of the same function — different
//! `match` arms, different `let` bindings, the two sides of an `if` —
//! have made disjoint changes that compose cleanly. Git can't know a
//! hunk is a match arm; working on the canonical AST, we can.
//!
//! [`merge_bodies`] is a pure structural three-way merge of three
//! [`CExpr`]s (base / ours / theirs). It auto-merges when the two edit
//! sets touch disjoint subtrees and reports a [`BodyMerge::Conflict`]
//! when they overlap or when the shapes can't be aligned. It makes no
//! type judgement — that's the caller's job (the store type-checks the
//! merged body through the same gate every other write goes through);
//! a body that merges structurally but not by type is still a conflict.
//!
//! The algorithm is the classic recursive three-way merge:
//!
//! * `ours == theirs` → both sides made the same edit; take it.
//! * `base == ours`   → only theirs changed; take theirs.
//! * `base == theirs` → only ours changed; take ours.
//! * otherwise both changed differently: recurse into the children if
//!   all three nodes are the same shape and their children align 1:1,
//!   merging child-by-child; a conflict in any child, a shape
//!   mismatch, or a length mismatch that can't be aligned, conflicts
//!   the whole node.
//!
//! Length-changing edits (an inserted arg, a deleted statement) on a
//! node *both* sides also edited are conservatively a conflict in this
//! slice: aligning insertions/deletions across two sides is the
//! `recursive`-strategy territory the op-log notes as future work. The
//! common, valuable case — disjoint edits to same-arity structures —
//! merges.

use lex_ast::{Arm, CExpr, RecordField};

/// Result of a three-way body merge.
#[derive(Debug, Clone, PartialEq)]
pub enum BodyMerge {
    /// The two edit sets composed; here is the merged expression.
    Merged(CExpr),
    /// The edits overlap (or the shapes can't be aligned). The caller
    /// falls back to a whole-function `ModifyModify` conflict.
    Conflict,
}

/// Three-way merge of a function body. See the module docs.
pub fn merge_bodies(base: &CExpr, ours: &CExpr, theirs: &CExpr) -> BodyMerge {
    match merge3(base, ours, theirs) {
        Some(e) => BodyMerge::Merged(e),
        None => BodyMerge::Conflict,
    }
}

/// The recursive core: `None` is a conflict, `Some` the merged node.
fn merge3(base: &CExpr, ours: &CExpr, theirs: &CExpr) -> Option<CExpr> {
    // Fast paths — hold for *every* node kind, leaves included.
    if ours == theirs {
        return Some(ours.clone());
    }
    if base == ours {
        return Some(theirs.clone());
    }
    if base == theirs {
        return Some(ours.clone());
    }

    // Both sides changed this node, differently. Only same-shape nodes
    // with 1:1-alignable children can be merged further.
    use CExpr::*;
    match (base, ours, theirs) {
        (
            Call { callee: cb, args: ab },
            Call { callee: co, args: ao },
            Call { callee: ct, args: at },
        ) => Some(Call {
            callee: Box::new(merge3(cb, co, ct)?),
            args: merge3_seq(ab, ao, at)?,
        }),

        (
            Let { name: nb, ty: tb, value: vb, body: bb },
            Let { name: no, ty: to, value: vo, body: bo },
            Let { name: nt, ty: tt, value: vt, body: bt },
        ) => Some(Let {
            name: pick3(nb, no, nt)?,
            ty: pick3(tb, to, tt)?,
            value: Box::new(merge3(vb, vo, vt)?),
            body: Box::new(merge3(bb, bo, bt)?),
        }),

        (
            Match { scrutinee: sb, arms: amb },
            Match { scrutinee: so, arms: amo },
            Match { scrutinee: st, arms: amt },
        ) => Some(Match {
            scrutinee: Box::new(merge3(sb, so, st)?),
            arms: merge3_arms(amb, amo, amt)?,
        }),

        (
            Block { statements: sb, result: rb },
            Block { statements: so, result: ro },
            Block { statements: st, result: rt },
        ) => Some(Block {
            statements: merge3_seq(sb, so, st)?,
            result: Box::new(merge3(rb, ro, rt)?),
        }),

        (
            Constructor { name: nb, args: ab },
            Constructor { name: no, args: ao },
            Constructor { name: nt, args: at },
        ) => Some(Constructor {
            name: pick3(nb, no, nt)?,
            args: merge3_seq(ab, ao, at)?,
        }),

        (
            TupleLit { items: ib },
            TupleLit { items: io },
            TupleLit { items: it },
        ) => Some(TupleLit { items: merge3_seq(ib, io, it)? }),

        (
            ListLit { items: ib },
            ListLit { items: io },
            ListLit { items: it },
        ) => Some(ListLit { items: merge3_seq(ib, io, it)? }),

        (
            RecordLit { fields: fb },
            RecordLit { fields: fo },
            RecordLit { fields: ft },
        ) => Some(RecordLit { fields: merge3_fields(fb, fo, ft)? }),

        (
            FieldAccess { value: vb, field: fb },
            FieldAccess { value: vo, field: fo },
            FieldAccess { value: vt, field: ft },
        ) => Some(FieldAccess {
            value: Box::new(merge3(vb, vo, vt)?),
            field: pick3(fb, fo, ft)?,
        }),

        (
            BinOp { op: ob, lhs: lb, rhs: rb },
            BinOp { op: oo, lhs: lo, rhs: ro },
            BinOp { op: ot, lhs: lt, rhs: rt },
        ) => Some(BinOp {
            op: pick3(ob, oo, ot)?,
            lhs: Box::new(merge3(lb, lo, lt)?),
            rhs: Box::new(merge3(rb, ro, rt)?),
        }),

        (
            UnaryOp { op: ob, expr: eb },
            UnaryOp { op: oo, expr: eo },
            UnaryOp { op: ot, expr: et },
        ) => Some(UnaryOp {
            op: pick3(ob, oo, ot)?,
            expr: Box::new(merge3(eb, eo, et)?),
        }),

        (
            Return { value: vb },
            Return { value: vo },
            Return { value: vt },
        ) => Some(Return { value: Box::new(merge3(vb, vo, vt)?) }),

        (
            Lambda { params: pb, return_type: rtb, effects: eb, effect_row_var: rvb, body: bb },
            Lambda { params: po, return_type: rto, effects: eo, effect_row_var: rvo, body: bo },
            Lambda { params: pt, return_type: rtt, effects: et, effect_row_var: rvt, body: bt },
        ) => Some(Lambda {
            params: pick3(pb, po, pt)?,
            return_type: pick3(rtb, rto, rtt)?,
            effects: pick3(eb, eo, et)?,
            effect_row_var: pick3(rvb, rvo, rvt)?,
            body: Box::new(merge3(bb, bo, bt)?),
        }),

        // Leaves (Literal, Var) reach here only when both sides changed
        // them differently — an overlap, so a conflict. Mismatched
        // node kinds are also a conflict: a subtree replaced on one
        // side and edited on the other can't be composed structurally.
        _ => None,
    }
}

/// Merge three same-length sequences element-wise. A length difference
/// on a node both sides edited is a conflict (see module docs).
fn merge3_seq(base: &[CExpr], ours: &[CExpr], theirs: &[CExpr]) -> Option<Vec<CExpr>> {
    if base.len() != ours.len() || base.len() != theirs.len() {
        return None;
    }
    let mut out = Vec::with_capacity(base.len());
    for i in 0..base.len() {
        out.push(merge3(&base[i], &ours[i], &theirs[i])?);
    }
    Some(out)
}

/// Merge three arm lists: same arm count, patterns picked three-way
/// (an arm whose *pattern* both sides changed differently conflicts),
/// bodies merged recursively. Two agents editing different arms is the
/// case this exists for.
fn merge3_arms(base: &[Arm], ours: &[Arm], theirs: &[Arm]) -> Option<Vec<Arm>> {
    if base.len() != ours.len() || base.len() != theirs.len() {
        return None;
    }
    let mut out = Vec::with_capacity(base.len());
    for i in 0..base.len() {
        out.push(Arm {
            pattern: pick3(&base[i].pattern, &ours[i].pattern, &theirs[i].pattern)?,
            body: merge3(&base[i].body, &ours[i].body, &theirs[i].body)?,
        });
    }
    Some(out)
}

/// Merge three record-field lists: same length and field names in the
/// same order (record fields are canonicalized to a stable order), each
/// value merged recursively.
fn merge3_fields(
    base: &[RecordField],
    ours: &[RecordField],
    theirs: &[RecordField],
) -> Option<Vec<RecordField>> {
    if base.len() != ours.len() || base.len() != theirs.len() {
        return None;
    }
    let mut out = Vec::with_capacity(base.len());
    for i in 0..base.len() {
        let name = pick3(&base[i].name, &ours[i].name, &theirs[i].name)?;
        out.push(RecordField {
            name,
            value: merge3(&base[i].value, &ours[i].value, &theirs[i].value)?,
        });
    }
    Some(out)
}

/// Three-way pick for a non-recursive field: same rule as the node
/// fast path. `None` when both sides changed it to different values.
fn pick3<T: PartialEq + Clone>(base: &T, ours: &T, theirs: &T) -> Option<T> {
    if ours == theirs {
        Some(ours.clone())
    } else if base == ours {
        Some(theirs.clone())
    } else if base == theirs {
        Some(ours.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a single fn from source and return its canonical body.
    fn body(src: &str) -> CExpr {
        let prog = lex_syntax::parse_source(src).expect("parse");
        let stages = lex_ast::canonicalize_program(&prog);
        for st in stages {
            if let lex_ast::Stage::FnDecl(fd) = st {
                return fd.body;
            }
        }
        panic!("no fn in source");
    }

    fn merged(base: &str, ours: &str, theirs: &str) -> CExpr {
        match merge_bodies(&body(base), &body(ours), &body(theirs)) {
            BodyMerge::Merged(e) => e,
            BodyMerge::Conflict => panic!("expected a clean merge, got Conflict"),
        }
    }

    fn is_conflict(base: &str, ours: &str, theirs: &str) -> bool {
        matches!(merge_bodies(&body(base), &body(ours), &body(theirs)), BodyMerge::Conflict)
    }

    #[test]
    fn one_sided_change_takes_that_side() {
        // ours changed, theirs didn't → take ours; and vice-versa.
        let base = "fn f(x :: Int) -> Int { x }\n";
        let ours = "fn f(x :: Int) -> Int { x + 1 }\n";
        assert_eq!(merged(base, ours, base), body(ours));
        assert_eq!(merged(base, base, ours), body(ours));
    }

    #[test]
    fn identical_edit_both_sides_is_not_a_conflict() {
        let base = "fn f(x :: Int) -> Int { x }\n";
        let same = "fn f(x :: Int) -> Int { x + 1 }\n";
        assert_eq!(merged(base, same, same), body(same));
    }

    #[test]
    fn disjoint_match_arms_auto_merge() {
        // The headline case: ours edits arm A's body, theirs edits arm
        // B's body. Git conflicts; we compose.
        let base = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 10,
    _ => 20,
  }
}
";
        let ours = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 11,
    _ => 20,
  }
}
";
        let theirs = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 10,
    _ => 22,
  }
}
";
        let want = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 11,
    _ => 22,
  }
}
";
        assert_eq!(merged(base, ours, theirs), body(want));
    }

    #[test]
    fn same_match_arm_edited_both_sides_conflicts() {
        let base = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 10,
    _ => 20,
  }
}
";
        let ours = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 11,
    _ => 20,
  }
}
";
        let theirs = "\
fn classify(n :: Int) -> Int {
  match n {
    0 => 12,
    _ => 20,
  }
}
";
        assert!(is_conflict(base, ours, theirs));
    }

    #[test]
    fn disjoint_let_bindings_auto_merge() {
        // ours edits the value binding, theirs edits the body — two
        // different subtrees of the same `let`.
        let base = "fn f(x :: Int) -> Int {\n  let y := x\n  y\n}\n";
        let ours = "fn f(x :: Int) -> Int {\n  let y := x + 1\n  y\n}\n";
        let theirs = "fn f(x :: Int) -> Int {\n  let y := x\n  y + 100\n}\n";
        let want = "fn f(x :: Int) -> Int {\n  let y := x + 1\n  y + 100\n}\n";
        assert_eq!(merged(base, ours, theirs), body(want));
    }

    #[test]
    fn disjoint_binop_operands_auto_merge() {
        // ours edits the lhs, theirs edits the rhs.
        let base = "fn f(x :: Int) -> Int { x + x }\n";
        let ours = "fn f(x :: Int) -> Int { (x + 1) + x }\n";
        let theirs = "fn f(x :: Int) -> Int { x + (x + 2) }\n";
        let want = "fn f(x :: Int) -> Int { (x + 1) + (x + 2) }\n";
        assert_eq!(merged(base, ours, theirs), body(want));
    }

    #[test]
    fn a_length_changing_edit_on_a_both_edited_node_conflicts() {
        // ours appends an arg to a call; theirs edits an existing arg.
        // Aligning an insertion against an edit is out of scope for
        // this slice → conflict (safe fallback).
        let base = "fn f(x :: Int) -> Int { g(x, x) }\nfn g(a :: Int, b :: Int) -> Int { a }\n";
        let ours = "fn f(x :: Int) -> Int { g(x, x, x) }\nfn g(a :: Int, b :: Int) -> Int { a }\n";
        let theirs = "fn f(x :: Int) -> Int { g(x, x + 9) }\nfn g(a :: Int, b :: Int) -> Int { a }\n";
        assert!(is_conflict(base, ours, theirs));
    }

    #[test]
    fn kind_replaced_one_side_edited_other_conflicts() {
        // ours replaces the whole body with a different node kind;
        // theirs edits inside the original → can't compose.
        let base = "fn f(x :: Int) -> Int { x + x }\n";
        let ours = "fn f(x :: Int) -> Int { 42 }\n";
        let theirs = "fn f(x :: Int) -> Int { x + (x + 1) }\n";
        assert!(is_conflict(base, ours, theirs));
    }
}
