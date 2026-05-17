//! Behavioral evaluation of signature-level `examples { ... }` blocks (#369 slice 2).
//!
//! Slice 1 (PR #370) shipped the AST + parser + type-checking of example args
//! and expected values against the function's signature. This pass takes the
//! next step: it actually *runs* each example through the bytecode VM and
//! compares the result to the declared `expected` value. A mismatch becomes
//! a [`TypeError::ExampleMismatch`] with `rule_tag = "example-mismatch"`,
//! surfaced through the same structured-JSON error envelope as every other
//! `lex check` diagnostic.
//!
//! ## Implementation strategy
//!
//! For each pure function with non-empty examples, synthesize a small set of
//! zero-argument helper functions and append them as new stages alongside
//! the original program:
//!
//! - `__ex_<fn>_<K>_arg_<I>` returning the *I*th argument of case *K*.
//! - `__ex_<fn>_<K>_expected` returning the declared expected value of case *K*.
//!
//! Compile the augmented program to bytecode (the user's program plus the
//! helpers all see the same global scope), and for each case:
//!
//! 1. Call each `__ex_<fn>_<K>_arg_<I>` helper through the VM to get a
//!    runtime `Value` for the argument.
//! 2. Call the original function with those values to get the actual `Value`.
//! 3. Call `__ex_<fn>_<K>_expected` to get the declared `Value`.
//! 4. Compare the two via [`Value`]'s `PartialEq`. On mismatch, emit
//!    `ExampleMismatch` with pretty-printed `expected` and `got`.
//!
//! ## v1 restrictions
//!
//! - Generic functions (with `type_params`) are skipped — the helper
//!   synthesis would need to monomorphize. Examples on generic functions
//!   still get the slice-1 *type-level* checks; they just don't get
//!   *behavioral* checks. Worth a follow-up issue if examples on generics
//!   become a real need.
//! - Pure-only (already enforced by `ExamplesOnEffectfulFn` in slice 1).

use lex_ast as a;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_types::TypeError;

/// Run the behavioral-evaluation pass over `stages` and return any
/// `ExampleMismatch` errors discovered. Returns the empty vec when every
/// example case passes (or when there are no eligible cases).
///
/// Stages that fail VM execution (panics, step-limit, etc.) surface as
/// `ExampleMismatch` with a synthetic "got" string describing the failure.
/// We deliberately do not wrap them in a separate error variant so the
/// downstream JSON envelope and repair-loop wiring stays uniform.
pub fn evaluate_examples(stages: &[a::Stage]) -> Vec<TypeError> {
    let helpers = synthesize_helpers(stages);
    if helpers.cases.is_empty() {
        return Vec::new();
    }

    let mut augmented: Vec<a::Stage> = stages.to_vec();
    augmented.extend(helpers.helper_stages);

    let bc = compile_program(&augmented);
    let bc = std::sync::Arc::new(bc);

    let mut out = Vec::new();
    for case in &helpers.cases {
        match run_case(&bc, case) {
            CaseOutcome::Pass => {}
            CaseOutcome::Mismatch { expected, got } => {
                out.push(TypeError::ExampleMismatch {
                    at_node: "n_0".into(),
                    fn_name: case.fn_name.clone(),
                    case_index: case.case_index,
                    expected,
                    got,
                });
            }
            CaseOutcome::RuntimeError(msg) => {
                // Surface VM panics as ExampleMismatch so the user sees a
                // clear "this example failed" diagnostic with the panic
                // message in the `got` slot. Keeps the error envelope uniform.
                out.push(TypeError::ExampleMismatch {
                    at_node: "n_0".into(),
                    fn_name: case.fn_name.clone(),
                    case_index: case.case_index,
                    expected: "(declared value)".into(),
                    got: format!("runtime error: {msg}"),
                });
            }
        }
    }
    out
}

struct Helpers {
    helper_stages: Vec<a::Stage>,
    cases: Vec<Case>,
}

struct Case {
    fn_name: String,
    case_index: usize,
    arg_helpers: Vec<String>,
    expected_helper: String,
}

fn synthesize_helpers(stages: &[a::Stage]) -> Helpers {
    let mut helper_stages = Vec::new();
    let mut cases = Vec::new();

    for stage in stages {
        let a::Stage::FnDecl(fd) = stage else { continue };
        if fd.examples.is_empty() {
            continue;
        }
        // v1: skip generics. Helper synthesis would need to pick a
        // concrete instantiation; defer until there's a real need.
        if !fd.type_params.is_empty() {
            continue;
        }
        // Pure-only is already enforced by ExamplesOnEffectfulFn in slice 1,
        // but we double-check here so a future regression doesn't lead the
        // VM to invoke a real effect handler during `lex check`.
        if !fd.effects.is_empty() {
            continue;
        }
        for (k, ex) in fd.examples.iter().enumerate() {
            let mut arg_helpers = Vec::with_capacity(ex.args.len());
            for (i, arg) in ex.args.iter().enumerate() {
                let helper_name = format!("__ex_{}_{}_arg_{}", fd.name, k, i);
                helper_stages.push(zero_arg_helper(&helper_name, fd.params[i].ty.clone(), arg.clone()));
                arg_helpers.push(helper_name);
            }
            let expected_helper = format!("__ex_{}_{}_expected", fd.name, k);
            helper_stages.push(zero_arg_helper(
                &expected_helper,
                fd.return_type.clone(),
                ex.expected.clone(),
            ));
            cases.push(Case {
                fn_name: fd.name.clone(),
                case_index: k,
                arg_helpers,
                expected_helper,
            });
        }
    }

    Helpers { helper_stages, cases }
}

fn zero_arg_helper(name: &str, return_type: a::TypeExpr, body: a::CExpr) -> a::Stage {
    a::Stage::FnDecl(a::FnDecl {
        name: name.into(),
        type_params: Vec::new(),
        params: Vec::new(),
        effects: Vec::new(),
        return_type,
        body,
        examples: Vec::new(),
    })
}

enum CaseOutcome {
    Pass,
    Mismatch { expected: String, got: String },
    RuntimeError(String),
}

fn run_case(bc: &std::sync::Arc<lex_bytecode::Program>, case: &Case) -> CaseOutcome {
    // Each VM invocation is a fresh instance — they share no state, and
    // because we restrict to pure functions, there's nothing to share.
    let mut arg_values: Vec<Value> = Vec::with_capacity(case.arg_helpers.len());
    for helper in &case.arg_helpers {
        match call_zero_arg(bc, helper) {
            Ok(v) => arg_values.push(v),
            Err(e) => return CaseOutcome::RuntimeError(format!("computing arg from `{helper}`: {e}")),
        }
    }
    let expected = match call_zero_arg(bc, &case.expected_helper) {
        Ok(v) => v,
        Err(e) => return CaseOutcome::RuntimeError(format!("computing expected from `{}`: {e}", case.expected_helper)),
    };
    let got = match call_with_args(bc, &case.fn_name, arg_values) {
        Ok(v) => v,
        Err(e) => return CaseOutcome::RuntimeError(format!("calling `{}`: {e}", case.fn_name)),
    };
    if expected == got {
        CaseOutcome::Pass
    } else {
        CaseOutcome::Mismatch {
            expected: pretty_value(&expected),
            got: pretty_value(&got),
        }
    }
}

fn call_zero_arg(bc: &std::sync::Arc<lex_bytecode::Program>, name: &str) -> Result<Value, String> {
    call_with_args(bc, name, Vec::new())
}

fn call_with_args(
    bc: &std::sync::Arc<lex_bytecode::Program>,
    name: &str,
    args: Vec<Value>,
) -> Result<Value, String> {
    let handler = DefaultHandler::new(Policy::pure()).with_program(std::sync::Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    // Defensive: cap steps so a runaway example can't hang `lex check`.
    vm.set_step_limit(1_000_000);
    vm.call(name, args).map_err(|e| format!("{e:?}"))
}

/// Pretty-print a `Value` for inclusion in `ExampleMismatch` errors.
/// We want the JSON envelope to carry something a human and an LLM can
/// both read; `Debug` is verbose but unambiguous and matches the rest of
/// Lex's diagnostic style.
fn pretty_value(v: &Value) -> String {
    match v {
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Unit => "()".into(),
        Value::List(xs) => format!(
            "[{}]",
            xs.iter().map(pretty_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Tuple(xs) => format!(
            "({})",
            xs.iter().map(pretty_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Variant { name, args } if args.is_empty() => name.clone(),
        Value::Variant { name, args } => format!(
            "{}({})",
            name,
            args.iter().map(pretty_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Record { fields: fs, .. } => format!(
            "{{ {} }}",
            fs.iter()
                .map(|(k, v)| format!("{k}: {}", pretty_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => format!("{other:?}"),
    }
}
