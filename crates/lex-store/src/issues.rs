//! Typed-issue acceptance evaluation and the `IssueVerified` attestation
//! (#949 phase 2) — the definition of done as code.
//!
//! An issue declares an oracle ([`lex_vcs::Acceptance`]); this module
//! evaluates it at a head and records the verdict as a content-addressed
//! attestation keyed by the *issue id*. Done is **evaluated, never
//! declared**: a passing verdict is only ever recorded when the gate
//! actually checked the oracle. The free-form shape is human-closed and the
//! metric/evidence oracles land later (#954); both record `Inconclusive`,
//! not `Passed`.
//!
//! Two deliberate boundaries keep this crate small:
//!
//! - **No parser.** Example-bearing shapes take their examples already
//!   parsed (`(fn_name, `[`lex_ast::Example`]`)`); the caller (lex-cli /
//!   lex-api, which have lex-syntax) turns the issue's example strings into
//!   ASTs by parsing them under a stub `fn` (the parser keeps a case's args
//!   and expected value, not its callee).
//! - **No runtime.** Running examples needs lex-runtime, which would be a
//!   dependency cycle. So this module *prepares* the program to run
//!   ([`prepare_example_stages`]) and the caller runs
//!   `lex_runtime::evaluate_examples` on it — the same split
//!   `record_examples_passed` already has with `lex publish`.
//!
//! Single-file heads only for now (stage-level multi-file de-mangling is
//! #942). A non-inlined head whose examples call an external dependency
//! can't be *run* without runtime linking (#946); the caller reports that
//! honestly as `Failed` with the VM's detail.

use std::collections::BTreeMap;

use lex_vcs::{
    render_signature, render_type_signature, Acceptance, ApiChangeKind, ApiEntry, Attestation,
    AttestationId, AttestationKind, AttestationResult, Issue, ProducerDescriptor,
};

use crate::render::demangled_head_stages;
use crate::store::{Store, StoreError};

/// The outcome of evaluating an issue's acceptance at a head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueEvaluation {
    /// The declared oracle holds.
    Passed,
    /// It doesn't; `detail` says what.
    Failed { detail: String },
    /// The shape isn't machine-evaluable here — free-form (human-closed), or
    /// an oracle whose evaluator lands later. Recorded as `Inconclusive`,
    /// never as passed.
    NotEvaluable { reason: String },
}

impl IssueEvaluation {
    pub fn failed(detail: impl Into<String>) -> Self {
        IssueEvaluation::Failed { detail: detail.into() }
    }
    pub fn not_evaluable(reason: impl Into<String>) -> Self {
        IssueEvaluation::NotEvaluable { reason: reason.into() }
    }
    pub fn is_passed(&self) -> bool {
        matches!(self, IssueEvaluation::Passed)
    }
}

/// Evaluate everything about `issue`'s acceptance that needs no execution.
///
/// - `typed_delta`: the declared API entries against the head (and base,
///   when the issue names one) — see [`check_api_delta`]. The examples half
///   is the caller's, via [`prepare_example_stages`].
/// - `failing_example`: nothing static to check; the example run decides.
/// - `free_form`, `metric_invariant`, `evidence`: not evaluable here.
pub fn evaluate_static(
    store: &Store,
    issue: &Issue,
    head_op: &str,
) -> Result<IssueEvaluation, StoreError> {
    match &issue.acceptance {
        Acceptance::FreeForm {} => Ok(IssueEvaluation::not_evaluable(
            "free_form: human-closed, not machine-evaluable",
        )),
        Acceptance::MetricInvariant { .. } => Ok(IssueEvaluation::not_evaluable(
            "metric_invariant: the metric/invariant oracle evaluator lands in #954",
        )),
        Acceptance::Evidence { .. } => Ok(IssueEvaluation::not_evaluable(
            "evidence: the evidence oracle evaluator lands in #954",
        )),
        Acceptance::FailingExample { .. } => Ok(IssueEvaluation::Passed),
        Acceptance::TypedDelta { api, .. } => {
            check_api_delta(store, issue.base.as_deref(), head_op, api)
        }
    }
}

/// Check a typed delta's declared API entries against the head — and the
/// base, when given. Signatures compare *like with like*: the head's
/// declaration is rendered with [`render_signature`] /
/// [`render_type_signature`] (the same form `api-diff` and `lex propagate`
/// show an author), the entry's `signature` is everything after the
/// declaration's name in that form (`(a :: Int, b :: Int) -> Int`), and
/// whitespace is ignored on both sides.
///
/// - `added`: present at head with the declared signature; absent at base.
/// - `changed`: present at head with the declared signature; present at
///   base with a *different* one.
/// - `removed`: absent at head; present at base.
///
/// Base checks only run when the issue names a base head.
pub fn check_api_delta(
    store: &Store,
    base: Option<&str>,
    head_op: &str,
    api: &[ApiEntry],
) -> Result<IssueEvaluation, StoreError> {
    let head = surface(&demangled_head_stages(store, head_op)?);
    let base_surface = match base {
        Some(b) => Some(surface(&demangled_head_stages(store, b)?)),
        None => None,
    };
    let mut problems: Vec<String> = Vec::new();
    for e in api {
        let want = squash(&e.signature);
        let at_head = head
            .get(&e.name)
            .and_then(|r| tail_after_name(r, &e.name))
            .map(|t| squash(&t));
        let at_base = base_surface
            .as_ref()
            .and_then(|b| b.get(&e.name))
            .and_then(|r| tail_after_name(r, &e.name))
            .map(|t| squash(&t));
        match e.kind {
            ApiChangeKind::Added => {
                match &at_head {
                    None => problems.push(format!("`{}`: declared added, but absent at head", e.name)),
                    Some(h) if *h != want => problems.push(format!(
                        "`{}`: signature at head `{}` differs from declared `{}`",
                        e.name, h, want
                    )),
                    _ => {}
                }
                if at_base.is_some() {
                    problems.push(format!("`{}`: declared added, but already present at base", e.name));
                }
            }
            ApiChangeKind::Changed => {
                match &at_head {
                    None => problems.push(format!("`{}`: declared changed, but absent at head", e.name)),
                    Some(h) if *h != want => problems.push(format!(
                        "`{}`: signature at head `{}` differs from declared `{}`",
                        e.name, h, want
                    )),
                    _ => {}
                }
                if base_surface.is_some() {
                    match &at_base {
                        None => problems.push(format!("`{}`: declared changed, but absent at base", e.name)),
                        Some(b) if *b == want => problems.push(format!(
                            "`{}`: declared changed, but base already had this signature",
                            e.name
                        )),
                        _ => {}
                    }
                }
            }
            ApiChangeKind::Removed => {
                if at_head.is_some() {
                    problems.push(format!("`{}`: declared removed, but still present at head", e.name));
                }
                if base_surface.is_some() && at_base.is_none() {
                    problems.push(format!("`{}`: declared removed, but was absent at base", e.name));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(IssueEvaluation::Passed)
    } else {
        Ok(IssueEvaluation::failed(problems.join("; ")))
    }
}

/// The head's program with every pre-existing example stripped and the
/// given `(fn_name, example)` cases attached — so that running it judges
/// exactly the issue's examples and nothing else. The caller runs
/// `lex_runtime::evaluate_examples` on the result; an empty error list
/// means the issue's examples pass at this head.
pub fn prepare_example_stages(
    store: &Store,
    head_op: &str,
    cases: &[(String, lex_ast::Example)],
) -> Result<Vec<lex_ast::Stage>, StoreError> {
    let mut stages = demangled_head_stages(store, head_op)?;
    for st in &mut stages {
        if let lex_ast::Stage::FnDecl(fd) = st {
            fd.examples.clear();
        }
    }
    for (name, example) in cases {
        let target = stages.iter_mut().find_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) if &fd.name == name => Some(fd),
            _ => None,
        });
        match target {
            Some(fd) => fd.examples.push(example.clone()),
            None => return Err(StoreError::IssueTarget(name.clone())),
        }
    }
    Ok(stages)
}

/// Record the verdict as an `IssueVerified` attestation keyed by the issue
/// id at `head_op`. `Passed` → `Passed`; `Failed` → `Failed`;
/// `NotEvaluable` → `Inconclusive` — never a pass the gate didn't check.
pub fn record_issue_verdict(
    store: &Store,
    issue: &Issue,
    head_op: &str,
    evaluation: &IssueEvaluation,
) -> Result<AttestationId, StoreError> {
    let result = match evaluation {
        IssueEvaluation::Passed => AttestationResult::Passed,
        IssueEvaluation::Failed { detail } => AttestationResult::Failed { detail: detail.clone() },
        IssueEvaluation::NotEvaluable { reason } => {
            AttestationResult::Inconclusive { detail: reason.clone() }
        }
    };
    let attestation = Attestation::new(
        issue.issue_id.clone(),
        Some(head_op.to_string()),
        None,
        AttestationKind::IssueVerified {
            issue_id: issue.issue_id.clone(),
            shape: issue.acceptance.shape().to_string(),
        },
        result,
        issue_gate_producer(),
        None,
    );
    store.attestation_log()?.put(&attestation)?;
    Ok(attestation.attestation_id.clone())
}

/// Every `IssueVerified` attestation recorded for `issue_id`.
pub fn issue_verdicts(store: &Store, issue_id: &str) -> Result<Vec<Attestation>, StoreError> {
    let all = store.attestation_log()?.list_for_stage(&issue_id.to_string())?;
    Ok(all
        .into_iter()
        .filter(|a| matches!(a.kind, AttestationKind::IssueVerified { .. }))
        .collect())
}

/// Whether the issue's *latest* verdict is a pass — the derived "done"
/// state (phase 3 builds the full open/in-progress/verified/blocked view on
/// this).
pub fn is_verified(store: &Store, issue_id: &str) -> Result<bool, StoreError> {
    let latest = issue_verdicts(store, issue_id)?
        .into_iter()
        .max_by_key(|a| a.timestamp);
    Ok(matches!(latest.map(|a| a.result), Some(AttestationResult::Passed)))
}

fn issue_gate_producer() -> ProducerDescriptor {
    ProducerDescriptor {
        tool: "lex-store::issue-gate".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// bare name → rendered declaration, for a de-mangled program.
fn surface(stages: &[lex_ast::Stage]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for st in stages {
        match st {
            lex_ast::Stage::FnDecl(fd) => {
                out.insert(fd.name.clone(), render_signature(fd));
            }
            lex_ast::Stage::TypeDecl(td) => {
                out.insert(td.name.clone(), render_type_signature(td));
            }
            lex_ast::Stage::Import(_) => {}
        }
    }
    out
}

/// Everything after the declaration's name in a rendered signature:
/// `fn gcd(a :: Int, b :: Int) -> Int` → `(a :: Int, b :: Int) -> Int`.
fn tail_after_name(rendered: &str, name: &str) -> Option<String> {
    let rest = rendered
        .strip_prefix("fn ")
        .or_else(|| rendered.strip_prefix("type "))?;
    let tail = rest.strip_prefix(name)?;
    Some(tail.trim().to_string())
}

/// Whitespace is never semantic in a signature; drop it so
/// `(a::Int,b::Int)->Int` and `(a :: Int, b :: Int) -> Int` compare equal.
fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_strips_keyword_and_name() {
        assert_eq!(
            tail_after_name("fn gcd(a :: Int, b :: Int) -> Int", "gcd").as_deref(),
            Some("(a :: Int, b :: Int) -> Int")
        );
        assert_eq!(tail_after_name("type Shape = A | B", "Shape").as_deref(), Some("= A | B"));
        // Wrong name → no tail (never a false positive).
        assert_eq!(tail_after_name("fn gcd(a :: Int) -> Int", "lcm"), None);
    }

    #[test]
    fn squash_ignores_whitespace_only() {
        assert_eq!(squash("(a :: Int, b :: Int) -> Int"), squash("(a::Int,b::Int)->Int"));
        assert_ne!(squash("(a :: Int) -> Int"), squash("(a :: Str) -> Int"));
    }
}
