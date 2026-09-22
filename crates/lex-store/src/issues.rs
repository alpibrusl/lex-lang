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

use std::collections::{BTreeMap, BTreeSet};

use lex_vcs::{
    render_signature, render_type_signature, Acceptance, ApiChangeKind, ApiEntry, Attestation,
    AcceptanceProposal, AttestationId, AttestationKind, AttestationResult, IntentLog, Issue,
    IssueId, IssueLog, OpLog, ProducerDescriptor, ProposalId, ReviewVerdict,
};
use serde::Serialize;

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

// ---- Derived state (#949 phase 3) ------------------------------------
//
// A board is a *view* over the op-log and the issue graph; nobody drags
// cards. State is computed here, never stored, so it cannot drift from the
// code.

/// Where an issue stands, computed from the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    /// No op carries this issue's intent yet.
    Open,
    /// Some op on some branch carries its intent (work has started).
    InProgress,
    /// Its latest verdict is a pass — done, as a proof.
    Verified,
    /// A dependency is not verified (an unknown dependency id counts as
    /// blocking, conservatively).
    Blocked,
}

/// An issue with its derived state.
#[derive(Debug, Clone, Serialize)]
pub struct IssueStatus {
    pub issue: Issue,
    pub state: IssueState,
    /// Dependencies not yet verified.
    pub blocked_on: Vec<IssueId>,
    /// Whether any op in the store carries this issue's intent.
    pub has_work: bool,
}

/// Every issue id that some op's intent references, across every branch's
/// history — the "work has started" signal, derived from provenance.
pub fn issues_in_progress(store: &Store) -> Result<BTreeSet<IssueId>, StoreError> {
    let log = OpLog::open(store.root())?;
    let intents = IntentLog::open(store.root())?;
    let mut out = BTreeSet::new();
    let mut seen_intents: BTreeSet<String> = BTreeSet::new();
    for branch in store.list_branches()? {
        // A branch whose record can't be read contributes no provenance;
        // skip it rather than failing the whole derivation (the review and
        // branch-head surfaces tolerate the same way).
        let Some(head) = store.get_branch(&branch).ok().flatten().and_then(|b| b.head_op) else {
            continue;
        };
        for rec in log.walk_forward(&head, None)? {
            let Some(iid) = rec.op.intent_id.clone() else { continue };
            if !seen_intents.insert(iid.clone()) {
                continue;
            }
            if let Some(intent) = intents.get(&iid)? {
                if let Some(issue_id) = intent.issue_id {
                    out.insert(issue_id);
                }
            }
        }
    }
    Ok(out)
}

/// One issue's derived state. Precedence: **verified** (done is done) →
/// **blocked** (a dependency isn't verified — work can't complete) →
/// **in progress** (an op carries its intent) → **open**.
pub fn issue_status(
    store: &Store,
    issue: &Issue,
    in_progress: &BTreeSet<IssueId>,
) -> Result<IssueStatus, StoreError> {
    let verified = is_verified(store, &issue.issue_id)?;
    let mut blocked_on = Vec::new();
    for dep in &issue.deps {
        if !is_verified(store, dep)? {
            blocked_on.push(dep.clone());
        }
    }
    let has_work = in_progress.contains(&issue.issue_id);
    let state = if verified {
        IssueState::Verified
    } else if !blocked_on.is_empty() {
        IssueState::Blocked
    } else if has_work {
        IssueState::InProgress
    } else {
        IssueState::Open
    };
    Ok(IssueStatus { issue: issue.clone(), state, blocked_on, has_work })
}

/// Derived state for every issue in the store, sorted by id.
pub fn all_issue_status(store: &Store) -> Result<Vec<IssueStatus>, StoreError> {
    let log = IssueLog::open(store.root())?;
    let in_progress = issues_in_progress(store)?;
    let mut out = Vec::new();
    for id in log.list_ids()? {
        if let Some(issue) = log.get(&id)? {
            out.push(issue_status(store, &issue, &in_progress)?);
        }
    }
    Ok(out)
}

// ---- Agent-refined acceptance (#956) ------------------------------------
//
// A proposal's verdict is a `Review` attestation keyed by the proposal id —
// the same intent-arbiter record `lex stage review` writes for a candidate,
// so an approval carries a named reviewer and lands in the same log. The
// issue is never rewritten (its id would change); its *effective*
// acceptance is derived: the latest approved proposal, else its own.

/// Where a proposal stands, derived from its latest review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
}

/// Only a free-form issue can be refined. A typed issue already has a
/// machine-checkable contract; changing it is a different issue, and
/// silently swapping the oracle under an existing id would let a verdict
/// mean something other than what the issue said.
pub fn check_refinable(issue: &Issue) -> Result<(), StoreError> {
    match issue.acceptance {
        Acceptance::FreeForm {} => Ok(()),
        ref a => Err(StoreError::IssueRefinement(format!(
            "issue {} is already {} — only a free_form issue takes a proposed acceptance; \
             file a new issue to change a typed contract",
            issue.issue_id,
            a.shape()
        ))),
    }
}

/// Record a human's verdict on a proposal. `approve` → `Review{Approve}` /
/// `Passed`; otherwise `Review{Reject}` / `Failed`. A later review
/// supersedes an earlier one.
pub fn record_proposal_review(
    store: &Store,
    proposal: &AcceptanceProposal,
    reviewer: &str,
    approve: bool,
    notes: Option<String>,
) -> Result<AttestationId, StoreError> {
    let (verdict, result) = if approve {
        (ReviewVerdict::Approve, AttestationResult::Passed)
    } else {
        (
            ReviewVerdict::Reject,
            AttestationResult::Failed {
                detail: notes.clone().unwrap_or_else(|| "proposal rejected".into()),
            },
        )
    };
    let attestation = Attestation::new(
        proposal.proposal_id.clone(),
        None,
        None,
        AttestationKind::Review { reviewer: reviewer.to_string(), verdict, notes },
        result,
        issue_gate_producer(),
        None,
    );
    store.attestation_log()?.put(&attestation)?;
    Ok(attestation.attestation_id.clone())
}

/// The latest review of a proposal, if any: `(approved?, timestamp)`.
fn latest_review(store: &Store, proposal_id: &ProposalId) -> Result<Option<(bool, u64)>, StoreError> {
    let latest = store
        .attestation_log()?
        .list_for_stage(proposal_id)?
        .into_iter()
        .filter_map(|a| match a.kind {
            AttestationKind::Review { verdict, .. } => {
                Some((verdict == ReviewVerdict::Approve, a.timestamp))
            }
            _ => None,
        })
        // Timestamps are whole seconds, so two reviews can tie; a tie
        // resolves to the rejection — never approve on ambiguity.
        .max_by_key(|(approved, ts)| (*ts, !*approved));
    Ok(latest)
}

pub fn proposal_status(store: &Store, proposal_id: &ProposalId) -> Result<ProposalStatus, StoreError> {
    Ok(match latest_review(store, proposal_id)? {
        None => ProposalStatus::Pending,
        Some((true, _)) => ProposalStatus::Approved,
        Some((false, _)) => ProposalStatus::Rejected,
    })
}

/// The acceptance the gate evaluates for `issue`: its most recently
/// approved proposal, or its own when none is approved. Returns the
/// proposal id that supplied it, if any.
pub fn effective_acceptance(
    store: &Store,
    issue: &Issue,
) -> Result<(Acceptance, Option<ProposalId>), StoreError> {
    let log = IssueLog::open(store.root())?;
    let mut best: Option<(u64, AcceptanceProposal)> = None;
    for p in log.proposals_for(&issue.issue_id)? {
        if let Some((true, ts)) = latest_review(store, &p.proposal_id)? {
            // Latest approval wins; a same-second tie breaks on the
            // proposal id so the answer never depends on directory order.
            if best
                .as_ref()
                .is_none_or(|(b, bp)| (ts, &p.proposal_id) > (*b, &bp.proposal_id))
            {
                best = Some((ts, p));
            }
        }
    }
    Ok(match best {
        Some((_, p)) => (p.acceptance, Some(p.proposal_id)),
        None => (issue.acceptance.clone(), None),
    })
}

/// `issue` with its acceptance replaced by the effective one — same id, so
/// verdicts, intents and the board still key on the issue itself.
pub fn with_effective_acceptance(store: &Store, issue: &Issue) -> Result<Issue, StoreError> {
    let (acceptance, _) = effective_acceptance(store, issue)?;
    Ok(Issue { acceptance, ..issue.clone() })
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
