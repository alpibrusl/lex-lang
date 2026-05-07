//! SMT-LIB exporter. Emits an SMT-LIB 2 script that an external Z3
//! (`z3 -smt2 file.smt`) can decide. This is text-only — we don't link
//! libz3 here to keep the dep surface light.
//!
//! We only translate the *structural* parts of a spec: quantifiers (as
//! universally-bound consts), arithmetic, comparison, boolean connectives.
//! Calls into the Lex stage are emitted as opaque uninterpreted
//! functions; an external Z3 won't be able to prove anything about them
//! without a hand-written axiom set. That's fine for the common case
//! where the property is a closed arithmetic formula.

use crate::ast::*;
use std::fmt::Write;

pub fn to_smtlib(spec: &Spec) -> String {
    let mut out = String::new();
    writeln!(out, "; spec: {}", spec.name).unwrap();
    writeln!(out, "(set-logic ALL)").unwrap();

    // Declare an uninterpreted function for any call target seen in the
    // body or constraints. Best-effort: assumes `Int` arity matching the
    // call site. Users can hand-edit before sending to Z3 if needed.
    let mut targets = std::collections::BTreeSet::new();
    collect_calls(&spec.body, &mut targets);
    for q in &spec.quantifiers {
        if let Some(c) = &q.constraint { collect_calls(c, &mut targets); }
    }
    for (name, arity) in targets {
        let args: Vec<&str> = std::iter::repeat_n("Int", arity).collect();
        writeln!(out, "(declare-fun {name} ({}) Int)", args.join(" ")).unwrap();
    }

    // Forall ... => body.
    writeln!(out).unwrap();
    write!(out, "(assert (not (forall (").unwrap();
    for (i, q) in spec.quantifiers.iter().enumerate() {
        if i > 0 { write!(out, " ").unwrap(); }
        write!(out, "({} {})", q.name, smt_type(&q.ty)).unwrap();
    }
    write!(out, ") ").unwrap();
    // Antecedent: AND of constraints (defaults to true).
    let antecedent = build_antecedent(spec);
    write!(out, "(=> {antecedent} ").unwrap();
    write!(out, "{}", expr_to_smt(&spec.body)).unwrap();
    writeln!(out, "))))").unwrap();

    writeln!(out, "(check-sat)").unwrap();
    writeln!(out, "(get-model)").unwrap();
    out
}

fn smt_type(t: &SpecType) -> &'static str {
    match t {
        SpecType::Int => "Int",
        SpecType::Float => "Real",
        SpecType::Bool => "Bool",
        SpecType::Str => "String",
        // #208: SMT-LIB encoding for record / list / named (ADT)
        // types isn't implemented. SMT supports all three via
        // datatype declarations + sequences, but mapping the spec
        // types into SMT-LIB needs name management and accessor
        // function generation. Out of scope; the SMT path stays
        // scalar-only. Specs that use these types are still
        // evaluable via the gate path (`evaluate_gate_compiled`),
        // which is what soft-agent uses.
        SpecType::Record { .. } => "<unsupported-Record>",
        SpecType::List { .. } => "<unsupported-List>",
        SpecType::Named { .. } => "<unsupported-Named>",
    }
}

fn build_antecedent(spec: &Spec) -> String {
    let parts: Vec<String> = spec.quantifiers.iter()
        .filter_map(|q| q.constraint.as_ref().map(expr_to_smt))
        .collect();
    if parts.is_empty() { "true".into() }
    else if parts.len() == 1 { parts.into_iter().next().unwrap() }
    else { format!("(and {})", parts.join(" ")) }
}

fn expr_to_smt(e: &SpecExpr) -> String {
    match e {
        SpecExpr::Var { name } => name.clone(),
        SpecExpr::IntLit { value } => value.to_string(),
        SpecExpr::FloatLit { value } => format!("{value}"),
        SpecExpr::BoolLit { value } => value.to_string(),
        SpecExpr::StrLit { value } => format!("\"{value}\""),
        SpecExpr::Not { expr } => format!("(not {})", expr_to_smt(expr)),
        SpecExpr::BinOp { op, lhs, rhs } => {
            let sop = match op {
                SpecOp::Add => "+", SpecOp::Sub => "-", SpecOp::Mul => "*",
                SpecOp::Div => "div", SpecOp::Mod => "mod",
                SpecOp::Eq => "=", SpecOp::Neq => "distinct",
                SpecOp::Lt => "<", SpecOp::Le => "<=",
                SpecOp::Gt => ">", SpecOp::Ge => ">=",
                SpecOp::And => "and", SpecOp::Or => "or",
            };
            format!("({sop} {} {})", expr_to_smt(lhs), expr_to_smt(rhs))
        }
        SpecExpr::Call { func, args } => {
            if args.is_empty() { func.clone() }
            else {
                let parts: Vec<String> = args.iter().map(expr_to_smt).collect();
                format!("({func} {})", parts.join(" "))
            }
        }
        SpecExpr::Let { name, value, body } => {
            format!("(let (({name} {})) {})", expr_to_smt(value), expr_to_smt(body))
        }
        // #208: see `smt_type`. Record field access maps to SMT-LIB
        // datatype accessors which need the datatype declarations
        // smt_type doesn't currently emit. List ops likewise map to
        // `seq.length` / `seq.nth` — same out-of-scope rationale.
        SpecExpr::FieldAccess { value, field } => {
            format!("(<unsupported-FieldAccess> {} {})", expr_to_smt(value), field)
        }
        SpecExpr::Index { list, index } => {
            format!("(<unsupported-Index> {} {})",
                expr_to_smt(list), expr_to_smt(index))
        }
        SpecExpr::Match { scrutinee, arms } => {
            // SMT-LIB match needs the datatype declared via declare-datatypes;
            // out of scope for v1 — same rationale as the other variants.
            let _ = arms;
            format!("(<unsupported-Match> {})", expr_to_smt(scrutinee))
        }
    }
}

fn collect_calls(e: &SpecExpr, out: &mut std::collections::BTreeSet<(String, usize)>) {
    match e {
        SpecExpr::Call { func, args } => {
            out.insert((func.clone(), args.len()));
            for a in args { collect_calls(a, out); }
        }
        SpecExpr::BinOp { lhs, rhs, .. } => {
            collect_calls(lhs, out);
            collect_calls(rhs, out);
        }
        SpecExpr::Not { expr } => collect_calls(expr, out),
        SpecExpr::Let { value, body, .. } => {
            collect_calls(value, out);
            collect_calls(body, out);
        }
        _ => {}
    }
}
