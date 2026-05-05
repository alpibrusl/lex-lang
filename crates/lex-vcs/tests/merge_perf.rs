//! Performance budget for #134: an agent harness can submit many
//! conflict resolutions in a single round-trip cheaply.
//!
//! The issue's full target is "50 resolutions in one round-trip,
//! < 500ms p99 against a 10k-op store." We measure the resolve
//! batch + commit at a smaller scale (50 conflicts in a 200-op
//! store) — what we're guarding against is a quadratic regression
//! in `MergeSession::resolve` or `commit`, not absolute throughput
//! at production scale. A criterion-based bench is the right
//! home for full-scale numbers; left as a follow-up.
//!
//! Budget: starting a session, resolving 50 conflicts in one
//! batch, and committing completes in < 250ms.

use std::collections::BTreeSet;
use std::time::Instant;
use tempfile::tempdir;

use lex_vcs::{
    apply, MergeSession, OpId, OpLog, Operation, OperationKind, Resolution, StageTransition,
};

const CONFLICT_COUNT: usize = 50;

#[test]
fn resolve_50_conflicts_in_one_batch_is_fast() {
    let tmp = tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();

    // 1) Common prefix: add CONFLICT_COUNT functions; the heads of
    //    that chain become the LCA.
    let mut head: Option<OpId> = None;
    for i in 0..CONFLICT_COUNT {
        let sig = format!("fn::sig_{i}");
        let stage = format!("base_{i}");
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.clone(),
                stage_id: stage.clone(),
                effects: BTreeSet::new(),
            },
            head.iter().cloned().collect::<Vec<_>>(),
        );
        let t = StageTransition::Create { sig_id: sig, stage_id: stage };
        let new = apply(&log, head.as_ref(), op, t).unwrap();
        head = Some(new.op_id);
    }
    let lca_head = head.clone();

    // 2) dst diverges: ModifyBody on each sig.
    let mut dst_head = lca_head.clone();
    for i in 0..CONFLICT_COUNT {
        let sig = format!("fn::sig_{i}");
        let from = format!("base_{i}");
        let to = format!("dst_{i}");
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.clone(),
                from_stage_id: from.clone(),
                to_stage_id: to.clone(),
            },
            dst_head.iter().cloned().collect::<Vec<_>>(),
        );
        let t = StageTransition::Replace { sig_id: sig, from, to };
        let new = apply(&log, dst_head.as_ref(), op, t).unwrap();
        dst_head = Some(new.op_id);
    }

    // 3) src diverges differently from the same LCA.
    let mut src_head = lca_head;
    for i in 0..CONFLICT_COUNT {
        let sig = format!("fn::sig_{i}");
        let from = format!("base_{i}");
        let to = format!("src_{i}");
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.clone(),
                from_stage_id: from.clone(),
                to_stage_id: to.clone(),
            },
            src_head.iter().cloned().collect::<Vec<_>>(),
        );
        let t = StageTransition::Replace { sig_id: sig, from, to };
        let new = apply(&log, src_head.as_ref(), op, t).unwrap();
        src_head = Some(new.op_id);
    }

    // 4) Time the merge round-trip.
    let start = Instant::now();
    let mut session = MergeSession::start(
        "perf-merge",
        &log,
        src_head.as_ref(),
        dst_head.as_ref(),
    ).unwrap();
    assert_eq!(session.remaining_conflicts().len(), CONFLICT_COUNT,
        "fixture should produce {CONFLICT_COUNT} conflicts");
    let pairs: Vec<(String, Resolution)> = (0..CONFLICT_COUNT)
        .map(|i| (format!("fn::sig_{i}"), Resolution::TakeTheirs))
        .collect();
    let verdicts = session.resolve(pairs);
    assert_eq!(verdicts.len(), CONFLICT_COUNT);
    assert!(verdicts.iter().all(|v| v.accepted));
    let _resolved = session.commit().expect("commit");
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 0.25,
        "{CONFLICT_COUNT}-conflict resolve+commit took {elapsed:?}; budget is < 250ms (issue #134)"
    );
}
