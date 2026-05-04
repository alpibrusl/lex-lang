//! Write-time type-check gate (#130).
//!
//! Wraps [`apply`](crate::apply::apply) with a type-checker pass.
//! When this gate is the only path that advances a branch head,
//! the store's invariant becomes "every accepted operation
//! produces a program that typechecks." The cascading-breakage
//! failure mode that breaks agentic workflows — agent A commits a
//! typing-broken stage, agent B reads it and builds work
//! assuming the broken stage, hours pass, CI catches the bug —
//! becomes impossible by construction.
//!
//! Effect violations surface here too: `lex-types::check_program`
//! reports an undeclared-effect call as a `TypeError` variant, so
//! a single rejection envelope covers both type and effect bugs.
//!
//! ## Performance
//!
//! The gate runs `lex_types::check_program` against the *candidate*
//! program — the sequence of `Stage`s that would exist after the
//! op is applied. Computing that sequence is the caller's job
//! (typically `Store::publish_program`, which already has it in
//! memory). The gate itself does not load anything from disk; it
//! just runs the type checker.
//!
//! Performance budget from #130: <50 ms p99 for a single-function
//! op on a 1000-stage store. This module doesn't validate that —
//! the budget belongs to the caller's candidate-assembly path
//! plus `lex_types::check_program`. We'll measure once the gate
//! is wired into a real `Store::publish_program` flow.

use lex_ast::Stage;
use lex_types::{check_program, TypeError};

use crate::apply::{apply, ApplyError, NewHead};
use crate::op_log::OpLog;
use crate::operation::{OpId, Operation, StageTransition};

#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The operation's parent or merge structure is wrong.
    /// Pass-through from [`ApplyError`]; same shape so callers
    /// already handling stale-parent / unknown-merge-parent on the
    /// raw apply path can keep their existing match arms.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The candidate program — i.e. the state that would exist
    /// after applying the op — doesn't typecheck. The op is *not*
    /// persisted; the branch head is unchanged.
    ///
    /// `op_id` is the would-be op_id (computed before the apply
    /// path persisted anything). Lets callers correlate the
    /// rejection with the op they submitted, even though no
    /// `<root>/ops/<op_id>.json` file exists.
    ///
    /// `errors` is the structured envelope `lex check` already
    /// emits. Effect violations show up here as a `TypeError`
    /// variant; the gate doesn't model them as a separate kind
    /// because the type checker doesn't either.
    #[error("type errors after applying op {op_id}: {} error(s)", errors.len())]
    TypeError {
        op_id: OpId,
        errors: Vec<TypeError>,
    },
}

/// Apply an operation only if the resulting candidate program
/// typechecks. Otherwise return [`GateError::TypeError`] with the
/// structured error envelope; nothing is persisted.
///
/// `candidate` is the sequence of `Stage`s that would exist after
/// the op is applied. The caller computes it — typically by
/// applying the op's [`StageTransition`] to the program it just
/// loaded from source. The gate does not assemble it from the
/// store; the cost of "load every stage" is on the caller, where
/// it can be amortized across the full publish flow.
pub fn check_and_apply(
    op_log: &OpLog,
    head_op: Option<&OpId>,
    op: Operation,
    transition: StageTransition,
    candidate: &[Stage],
) -> Result<NewHead, GateError> {
    if let Err(errors) = check_program(candidate) {
        return Err(GateError::TypeError {
            op_id: op.op_id(),
            errors,
        });
    }
    Ok(apply(op_log, head_op, op, transition)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::OperationKind;
    use std::collections::BTreeSet;

    fn fac_op_and_transition() -> (Operation, StageTransition) {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac".into(),
                stage_id: "s1".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        let t = StageTransition::Create {
            sig_id: "fac".into(),
            stage_id: "s1".into(),
        };
        (op, t)
    }

    fn parse(src: &str) -> Vec<Stage> {
        let prog = lex_syntax::parse_source(src).expect("parse");
        lex_ast::canonicalize_program(&prog)
    }

    #[test]
    fn clean_program_is_accepted_and_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let candidate = parse(
            "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
        );
        let (op, t) = fac_op_and_transition();
        let head = check_and_apply(&log, None, op, t, &candidate).unwrap();
        assert!(log.get(&head.op_id).unwrap().is_some());
    }

    #[test]
    fn type_error_is_rejected_and_nothing_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        // `not_defined` is referenced but never declared — the type
        // checker emits an `UnknownIdentifier` error.
        let candidate =
            parse("fn broken(x :: Int) -> Int { not_defined(x) }\n");
        let (op, t) = fac_op_and_transition();
        let expected_op_id = op.op_id();
        let err = check_and_apply(&log, None, op, t, &candidate)
            .expect_err("expected TypeError");
        match err {
            GateError::TypeError { op_id, errors } => {
                assert_eq!(op_id, expected_op_id);
                assert!(!errors.is_empty(), "expected at least one TypeError");
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        // The op record was NOT persisted on the rejection path —
        // the store's "always-valid HEAD" invariant holds.
        assert!(log.get(&expected_op_id).unwrap().is_none());
    }

    #[test]
    fn arity_mismatch_is_rejected() {
        // Calling `add` with one arg when it takes two should
        // produce an `ArityMismatch`. Verifies that several
        // `TypeError` variants flow through the gate, not just
        // unknown-identifier.
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let candidate = parse(
            "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn caller() -> Int { add(1) }\n",
        );
        let (op, t) = fac_op_and_transition();
        let err = check_and_apply(&log, None, op, t, &candidate)
            .expect_err("expected TypeError");
        assert!(matches!(err, GateError::TypeError { .. }));
    }

    #[test]
    fn parent_check_still_runs_when_program_is_clean() {
        // Pass a clean candidate but a stale-parent op. The gate
        // shouldn't accept it just because typechecking passed —
        // structural rejection still wins.
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let candidate = parse(
            "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
        );
        // First op lands cleanly so head_op is set.
        let (op1, t1) = fac_op_and_transition();
        let head1 = check_and_apply(&log, None, op1, t1, &candidate).unwrap();
        // Second op declares a wrong parent.
        let bogus = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fac".into(),
                from_stage_id: "s1".into(),
                to_stage_id: "s2".into(),
            },
            ["someone-else".into()],
        );
        let t = StageTransition::Replace {
            sig_id: "fac".into(),
            from: "s1".into(),
            to: "s2".into(),
        };
        let err = check_and_apply(&log, Some(&head1.op_id), bogus, t, &candidate)
            .expect_err("expected Apply(StaleParent)");
        match err {
            GateError::Apply(ApplyError::StaleParent { .. }) => {}
            other => panic!("expected Apply(StaleParent), got {other:?}"),
        }
    }
}
