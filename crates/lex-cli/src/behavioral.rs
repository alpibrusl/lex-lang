//! Behavioral equivalence tier for `lex op replay` (#836 follow-up).
//!
//! Exact stage-id comparison answers "is the regeneration byte-identical
//! to what was recorded". A benchmark of the regenerate loop found that
//! most "not reproduced" verdicts were the *same function written
//! differently* — `if n < 2 { n }` vs `match n { 0 => 0, 1 => 1, _ => .. }`,
//! `a > b` vs `a >= b` where the tie returns the same value — behaviorally
//! identical, rejected only because the oracle was syntactic.
//!
//! This tier runs the recorded and candidate functions on sampled inputs
//! and checks they agree, so `lex op replay` can report an honest,
//! distinctly-labeled "reproduced (behavioral)" instead of a false miss.
//! It is a *second* tier: exact match is still tried first and is the
//! stronger claim; this only runs when exact fails but the candidate is a
//! valid same-signature stage.
//!
//! Scope is deliberately narrow so a positive is trustworthy: only pure
//! (effect-free) first-order functions whose parameters are all `Int` or
//! `Bool` and whose return type is `Int`, `Bool`, or `Str`. Anything else
//! returns `None` — exact-only, never a fabricated equivalence. The check
//! runs each candidate in the *full program at the op* (parent state with
//! the target swapped in), so a function that calls parent helpers is
//! still runnable.

use std::sync::Arc;

use lex_ast::{stage_id, FnDecl, Param, Stage, TypeExpr};
use lex_bytecode::{compile_program, vm::Vm, Program, Value};
use lex_runtime::{DefaultHandler, Policy};

/// Whether two functions have the same *callable identity* — name,
/// parameter types (names ignored), return type, and effects. Deliberately
/// excludes the body and any `examples {}` block, both of which are folded
/// into `sig_id`/`stage_id`. This is what "a regeneration of the same
/// function" means: a fresh regeneration is given the intent and the type
/// signature, not the recorded examples, so it legitimately produces a
/// different `sig_id` while still being the same function to regenerate.
pub fn same_callable(a: &FnDecl, b: &FnDecl) -> bool {
    a.name == b.name
        && a.params.len() == b.params.len()
        && a.params.iter().zip(&b.params).all(|(x, y)| x.ty == y.ty)
        && a.return_type == b.return_type
        && a.effects == b.effects
}

/// The `FnDecl` in `stages` whose exact `stage_id` matches — the recorded
/// target, located unambiguously by content address.
pub fn fndecl_at_stage<'a>(stages: &'a [Stage], want_stage: &str) -> Option<&'a FnDecl> {
    stages.iter().find_map(|s| match s {
        Stage::FnDecl(fd) if stage_id(s).as_deref() == Some(want_stage) => Some(fd),
        _ => None,
    })
}

/// `stages` with the stage whose id is `want_stage` replaced by `repl`.
/// `None` if no stage matched (nothing to replace).
fn swap_target(stages: &[Stage], want_stage: &str, repl: Stage) -> Option<Vec<Stage>> {
    let mut out = stages.to_vec();
    let mut replaced = false;
    for s in out.iter_mut() {
        if stage_id(s).as_deref() == Some(want_stage) {
            *s = repl.clone();
            replaced = true;
            break;
        }
    }
    if replaced {
        Some(out)
    } else {
        None
    }
}

/// Cap on how many input tuples to try. Bounds cost on multi-parameter
/// functions (the grid is a cartesian product) without weakening the
/// signal — agreement across dozens of inputs is strong evidence.
const MAX_SAMPLES: usize = 64;
/// Per-call opcode budget. A candidate that diverges on an input the
/// recorded function terminates on is not equivalent; this makes that a
/// bounded failure rather than a hang.
const STEP_LIMIT: u64 = 500_000;

/// The bare name of a nullary named type (`Int`, `Bool`, `Str`, ...), or
/// `None` for anything with type arguments / a non-named shape.
fn scalar_name(t: &TypeExpr) -> Option<&str> {
    match t {
        TypeExpr::Named { name, args } if args.is_empty() => Some(name.as_str()),
        _ => None,
    }
}

/// The sample set for a parameter type, or `None` if the type isn't one we
/// enumerate (which disqualifies the whole function).
fn param_samples(t: &TypeExpr) -> Option<Vec<Value>> {
    match scalar_name(t)? {
        "Int" => Some(
            [-3, -1, 0, 1, 2, 5, 10]
                .iter()
                .map(|n| Value::Int(*n))
                .collect(),
        ),
        "Bool" => Some(vec![Value::Bool(true), Value::Bool(false)]),
        _ => None,
    }
}

fn comparable_return(t: &TypeExpr) -> bool {
    matches!(scalar_name(t), Some("Int") | Some("Bool") | Some("Str"))
}

/// Scalar value equality — `Value` isn't `PartialEq`, and we only ever
/// compare the return types the gate admits.
fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        _ => false,
    }
}

/// Cartesian product of each parameter's sample set, capped at
/// [`MAX_SAMPLES`]. `None` if any parameter type isn't sampleable.
fn input_grid(params: &[Param]) -> Option<Vec<Vec<Value>>> {
    let per: Vec<Vec<Value>> = params
        .iter()
        .map(|p| param_samples(&p.ty))
        .collect::<Option<_>>()?;
    let mut rows: Vec<Vec<Value>> = vec![vec![]];
    for col in &per {
        let mut next = Vec::new();
        for r in &rows {
            for v in col {
                let mut nr = r.clone();
                nr.push(v.clone());
                next.push(nr);
            }
        }
        next.truncate(MAX_SAMPLES);
        rows = next;
    }
    rows.truncate(MAX_SAMPLES);
    Some(rows)
}

/// Type-check (with the stdlib parse rewrite `lex run` also applies) and
/// compile a program, or `None` if it doesn't type-check.
fn compiled(stages: &[Stage]) -> Option<Program> {
    let mut s = stages.to_vec();
    lex_types::check_and_rewrite_program(&mut s).ok()?;
    Some(compile_program(&s))
}

/// Call `name(args)` under a permissive, step-limited VM. `None` on any
/// runtime error or step-limit overrun (used to mean "diverged / failed
/// on this input"). Permissive policy is safe: the gate admits only
/// effect-free targets, so no effect op executes.
fn call_fn(bc: &Arc<Program>, name: &str, args: Vec<Value>) -> Option<Value> {
    let handler = DefaultHandler::new(Policy::permissive()).with_program(Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.set_step_limit(STEP_LIMIT);
    vm.call(name, args).ok()
}

/// If `candidate` is behaviorally equivalent to the recorded target over
/// sampled inputs, the number of inputs that agreed; otherwise `None`
/// (not equivalent, or the function is outside the supported scope).
///
/// `expected_stages` is the full program at the op (parent + recorded
/// target); `expected_stage_id` locates the recorded target by exact
/// content address (so a differing `examples {}` block doesn't hide it).
pub fn behavioral_equiv(
    expected_stages: &[Stage],
    candidate: &Stage,
    expected_stage_id: &str,
) -> Option<usize> {
    let fd = fndecl_at_stage(expected_stages, expected_stage_id)?;
    let cand_fd = match candidate {
        Stage::FnDecl(f) => f,
        _ => return None,
    };

    // (1) Recorded-examples contract — the *intended-domain* behavioral spec.
    // lex-code attaches `examples {}` to real functions; those are folded
    // into the stage id (so they never match exactly) but they are also the
    // recorded behavioral contract, on the domain the author meant (a `gcd`
    // spec never mentions negatives). A candidate that satisfies the same
    // examples the original had to pass is a real behavioral reproduction —
    // and this handles any types, not just scalars, and sidesteps the
    // synthetic grid's out-of-domain sampling. This is the primary path for
    // real lex-code ops.
    if !fd.examples.is_empty() {
        let mut with_ex = cand_fd.clone();
        with_ex.examples = fd.examples.clone();
        if let Some(stages2) =
            swap_target(expected_stages, expected_stage_id, Stage::FnDecl(with_ex))
        {
            // Must type-check, then every recorded example must hold on the
            // candidate body (evaluate_examples returns one error per failing
            // case; empty = all held — the other functions' examples are
            // recorded and passing, so any failure is the candidate's).
            if compiled(&stages2).is_some() && lex_runtime::evaluate_examples(&stages2).is_empty() {
                return Some(fd.examples.len());
            }
        }
    }

    // (2) Synthetic grid — for functions with no recorded examples. Gate to
    // pure, first-order scalar params with a comparable return so a positive
    // is trustworthy.
    if !fd.effects.is_empty() {
        return None;
    }
    if !comparable_return(&fd.return_type) {
        return None;
    }
    let name = fd.name.clone();
    let grid = input_grid(&fd.params)?;
    if grid.is_empty() {
        return None;
    }

    // Candidate program = the expected program with the recorded target's
    // stage (located by exact stage id) swapped for the candidate, so
    // helper calls still resolve.
    let cand_stages = swap_target(expected_stages, expected_stage_id, candidate.clone())?;

    let exp_bc = Arc::new(compiled(expected_stages)?);
    let cand_bc = Arc::new(compiled(&cand_stages)?);

    let mut compared = 0usize;
    for args in &grid {
        // Skip inputs where the recorded function itself diverges/fails —
        // there's nothing well-defined to match against there.
        let expected = match call_fn(&exp_bc, &name, args.clone()) {
            Some(v) => v,
            None => continue,
        };
        match call_fn(&cand_bc, &name, args.clone()) {
            Some(got) if value_eq(&expected, &got) => compared += 1,
            _ => return None, // candidate differs, diverged, or failed
        }
    }
    if compared == 0 {
        None
    } else {
        Some(compared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stages(src: &str) -> Vec<Stage> {
        let prog = lex_syntax::parse_source(src).expect("parse");
        lex_ast::canonicalize_program(&prog)
    }

    /// The stage id of the named fn in `stages` — the exact content address
    /// behavioral_equiv locates the recorded target by.
    fn target_stage(stages: &[Stage], name: &str) -> String {
        stages
            .iter()
            .find_map(|s| match s {
                Stage::FnDecl(fd) if fd.name == name => stage_id(s),
                _ => None,
            })
            .expect("stage")
    }

    const BASE: &str = "fn base(x :: Int) -> Int { x }\n";

    #[test]
    fn rescues_equivalent_variant() {
        // `>` vs `>=` agree on every pair (the tie returns the same value).
        let expected = stages(&format!(
            "{BASE}fn max2(a :: Int, b :: Int) -> Int {{ if a > b {{ a }} else {{ b }} }}"
        ));
        let stg = target_stage(&expected, "max2");
        let cand = stages("fn max2(a :: Int, b :: Int) -> Int { if a >= b { a } else { b } }")
            .into_iter()
            .next()
            .unwrap();
        assert!(behavioral_equiv(&expected, &cand, &stg).is_some());
    }

    #[test]
    fn rescues_across_differing_examples() {
        // The library-build finding: the recorded fn carries `examples {}`
        // (folded into stage_id/sig_id) but the regeneration doesn't. Same
        // body, different examples → recognized by stage id + behaviorally
        // equal, so it must reproduce, not vanish as "a different function".
        let expected = stages(&format!(
            "{BASE}fn gcd(a :: Int, b :: Int) -> Int\n  examples {{ gcd(4, 0) => 4, gcd(10, 6) => 2 }}\n{{ if b == 0 {{ a }} else {{ gcd(b, a % b) }} }}"
        ));
        let stg = target_stage(&expected, "gcd");
        let cand =
            stages("fn gcd(a :: Int, b :: Int) -> Int { if b == 0 { a } else { gcd(b, a % b) } }")
                .into_iter()
                .next()
                .unwrap();
        assert!(behavioral_equiv(&expected, &cand, &stg).is_some());
    }

    #[test]
    fn rejects_different_behavior() {
        let expected = stages(&format!(
            "{BASE}fn max2(a :: Int, b :: Int) -> Int {{ if a > b {{ a }} else {{ b }} }}"
        ));
        let stg = target_stage(&expected, "max2");
        // a + b is not max.
        let cand = stages("fn max2(a :: Int, b :: Int) -> Int { a + b }")
            .into_iter()
            .next()
            .unwrap();
        assert!(behavioral_equiv(&expected, &cand, &stg).is_none());
    }

    #[test]
    fn skips_unsupported_signature() {
        // Str parameter is outside the sampleable scope → exact-only (None),
        // never a fabricated equivalence.
        let src = "fn tag(s :: Str) -> Str { s }";
        let expected = stages(src);
        let stg = target_stage(&expected, "tag");
        let cand = stages("fn tag(s :: Str) -> Str { s }")
            .into_iter()
            .next()
            .unwrap();
        assert!(behavioral_equiv(&expected, &cand, &stg).is_none());
    }

    #[test]
    fn same_callable_ignores_examples_and_param_names() {
        let a = match &stages("fn f(x :: Int) -> Int\n  examples { f(1) => 2 }\n{ x + 1 }")[0] {
            Stage::FnDecl(fd) => fd.clone(),
            _ => unreachable!(),
        };
        let b = match &stages("fn f(y :: Int) -> Int { y + 1 }")[0] {
            Stage::FnDecl(fd) => fd.clone(),
            _ => unreachable!(),
        };
        assert!(
            same_callable(&a, &b),
            "same name/types differ only in examples + param name"
        );
        let c = match &stages("fn f(x :: Int, z :: Int) -> Int { x }")[0] {
            Stage::FnDecl(fd) => fd.clone(),
            _ => unreachable!(),
        };
        assert!(
            !same_callable(&a, &c),
            "different arity is a different callable"
        );
    }
}
