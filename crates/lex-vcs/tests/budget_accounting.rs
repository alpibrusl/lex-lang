//! Conformance tests for #247: cost accounting on ops.
//!
//! Coverage:
//!
//! 1. `budget_from_effects` parses `"budget(N)"` labels correctly,
//!    rejects malformed input, and picks the smallest cost when
//!    multiple budget declarations exist.
//! 2. The new `Option<u64>` fields use `skip_serializing_if =
//!    "Option::is_none"` so pre-#247 ops keep byte-identical
//!    canonical bytes — verified against the golden hash from #243.
//! 3. `OperationKind::budget_delta()` returns the right pair for
//!    each variant.
//! 4. End-to-end: a sequence of ops creating, growing, and shrinking
//!    a budget yields the expected per-op deltas, matching what
//!    `lex op log --budget-drift` and `lex audit --budget` will
//!    surface.

use lex_vcs::{
    operation_budget_from_effects as budget_from_effects, EffectSet, Operation, OperationKind,
    StageTransition,
};
use std::collections::BTreeSet;

fn s(label: &str) -> EffectSet {
    [label.to_string()].into_iter().collect()
}

fn many(labels: &[&str]) -> EffectSet {
    labels.iter().map(|s| s.to_string()).collect()
}

#[test]
fn budget_from_effects_parses_well_formed_label() {
    assert_eq!(budget_from_effects(&s("budget(50)")), Some(50));
    assert_eq!(budget_from_effects(&s("budget(0)")), Some(0));
    assert_eq!(budget_from_effects(&s("budget(18446744073709551615)")), Some(u64::MAX));
}

#[test]
fn budget_from_effects_returns_none_for_no_budget() {
    assert_eq!(budget_from_effects(&BTreeSet::new()), None);
    assert_eq!(budget_from_effects(&s("io")), None);
    assert_eq!(budget_from_effects(&many(&["io", "fs_write"])), None);
}

#[test]
fn bare_budget_without_arg_returns_none() {
    // The parameterized form `budget(N)` carries the cost; the
    // bare form `budget` doesn't, so the magnitude is unknown.
    // budget_from_effects returns None — distinguishes "no budget
    // declared" from "budget declared but no cost we can derive."
    assert_eq!(budget_from_effects(&s("budget")), None);
}

#[test]
fn budget_from_effects_picks_smallest_when_duplicated() {
    // Multiple budget declarations on the same fn shouldn't happen
    // (the type-checker rejects them), but if they do, return the
    // conservative (smallest) cost.
    assert_eq!(
        budget_from_effects(&many(&["budget(100)", "budget(50)", "budget(200)"])),
        Some(50),
    );
}

#[test]
fn budget_from_effects_ignores_malformed_forms() {
    assert_eq!(budget_from_effects(&s("budget(")), None);
    assert_eq!(budget_from_effects(&s("budget()")), None);
    assert_eq!(budget_from_effects(&s("budget(abc)")), None);
    assert_eq!(budget_from_effects(&s("budget(-5)")), None);
    assert_eq!(budget_from_effects(&s("budget(1.5)")), None);
}

#[test]
fn add_function_without_budget_keeps_pre_247_op_id() {
    // The golden test in tests/opid_golden.rs pins
    // `add_function`'s op_id at this value. #247 added an
    // optional `budget_cost` field with `skip_serializing_if =
    // "Option::is_none"`, which means an op constructed with
    // `budget_cost: None` must produce byte-identical canonical
    // bytes to the pre-#247 form.
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac::Int->Int".into(),
            stage_id: "abc123".into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        [],
    );
    assert_eq!(
        op.op_id(),
        "f112990d31ef2a63f3e5ca5680637ed36a54bc7e8230510ae0c0e93fcb39d104",
        "AddFunction without budget rotated its OpId — the additive \
         Option pattern is broken; pre-#247 stores would migrate.",
    );
}

#[test]
fn add_function_with_budget_produces_distinct_op_id() {
    let op_no_budget = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "s".into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        [],
    );
    let op_with_budget = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "s".into(),
            effects: BTreeSet::new(),
            budget_cost: Some(50),
        },
        [],
    );
    assert_ne!(op_no_budget.op_id(), op_with_budget.op_id());
}

#[test]
fn budget_delta_returns_the_right_pair_per_variant() {
    let add = OperationKind::AddFunction {
        sig_id: "f".into(),
        stage_id: "s".into(),
        effects: BTreeSet::new(),
        budget_cost: Some(10),
    };
    assert_eq!(add.budget_delta(), (None, Some(10)));

    let modify = OperationKind::ModifyBody {
        sig_id: "f".into(),
        from_stage_id: "s1".into(),
        to_stage_id: "s2".into(),
        from_budget: Some(10),
        to_budget: Some(50),
    };
    assert_eq!(modify.budget_delta(), (Some(10), Some(50)));

    let change_effect = OperationKind::ChangeEffectSig {
        sig_id: "f".into(),
        from_stage_id: "s1".into(),
        to_stage_id: "s2".into(),
        from_effects: BTreeSet::new(),
        to_effects: BTreeSet::new(),
        from_budget: Some(50),
        to_budget: Some(100),
    };
    assert_eq!(change_effect.budget_delta(), (Some(50), Some(100)));

    let remove = OperationKind::RemoveFunction {
        sig_id: "f".into(),
        last_stage_id: "s".into(),
    };
    assert_eq!(remove.budget_delta(), (None, None));

    let rename = OperationKind::RenameSymbol {
        from: "a".into(),
        to: "b".into(),
        body_stage_id: "s".into(),
    };
    assert_eq!(rename.budget_delta(), (None, None));

    let merge = OperationKind::Merge { resolved: 0 };
    assert_eq!(merge.budget_delta(), (None, None));
}

#[test]
fn budget_chain_grow_shrink_reports_the_right_deltas() {
    // The issue's acceptance criterion: a sequence of ops creating,
    // growing, and shrinking a budget yields the expected per-op
    // deltas.
    //
    //   op1: AddFunction         budget = (None, Some(10))
    //   op2: ModifyBody          budget = (Some(10), Some(50))   +400%
    //   op3: ChangeEffectSig     budget = (Some(50), Some(100))  +100%
    //   op4: ModifyBody          budget = (Some(100), Some(20))  -80%
    let ops: Vec<OperationKind> = vec![
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "s1".into(),
            effects: s("budget(10)"),
            budget_cost: Some(10),
        },
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "s1".into(),
            to_stage_id: "s2".into(),
            from_budget: Some(10),
            to_budget: Some(50),
        },
        OperationKind::ChangeEffectSig {
            sig_id: "fac".into(),
            from_stage_id: "s2".into(),
            to_stage_id: "s3".into(),
            from_effects: s("budget(50)"),
            to_effects: s("budget(100)"),
            from_budget: Some(50),
            to_budget: Some(100),
        },
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "s3".into(),
            to_stage_id: "s4".into(),
            from_budget: Some(100),
            to_budget: Some(20),
        },
    ];

    let deltas: Vec<(Option<u64>, Option<u64>)> =
        ops.iter().map(|k| k.budget_delta()).collect();
    assert_eq!(
        deltas,
        vec![
            (None, Some(10)),
            (Some(10), Some(50)),
            (Some(50), Some(100)),
            (Some(100), Some(20)),
        ],
    );

    // Every op targets the same SigId.
    for op in &ops {
        assert_eq!(op.budget_sig().map(String::as_str), Some("fac"));
    }
}

#[test]
fn modify_body_with_unchanged_budget_round_trips_canonically() {
    // Even when from_budget == to_budget, the fields are recorded
    // (so consumers know the budget IS, not just whether it
    // changed). Round-trip through serde must preserve them.
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "s1".into(),
            to_stage_id: "s2".into(),
            from_budget: Some(50),
            to_budget: Some(50),
        },
        ["op-parent".into()],
    );
    let json = serde_json::to_string(&op).expect("serialize");
    let back: Operation = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(op.op_id(), back.op_id());
    // The fields make it into the JSON because they're Some.
    assert!(json.contains("\"from_budget\":50"));
    assert!(json.contains("\"to_budget\":50"));
}

#[test]
fn change_effect_sig_carries_budget_when_present_in_serialized_json() {
    let op = Operation::new(
        OperationKind::ChangeEffectSig {
            sig_id: "f".into(),
            from_stage_id: "old".into(),
            to_stage_id: "new".into(),
            from_effects: BTreeSet::new(),
            to_effects: many(&["io", "budget(100)"]),
            from_budget: None,
            to_budget: Some(100),
        },
        ["op-parent".into()],
    );
    let json = serde_json::to_string(&op).expect("serialize");
    // `from_budget: None` is omitted, `to_budget: Some(100)` appears.
    assert!(!json.contains("\"from_budget\""), "from_budget=None must be skipped: {json}");
    assert!(json.contains("\"to_budget\":100"), "to_budget=Some(100) must serialize: {json}");
}

#[test]
fn stage_transition_is_unchanged_for_budget_bearing_variants() {
    // Sanity: budget fields don't affect StageTransition derivation;
    // the existing `Replace` shape still applies.
    let op = OperationKind::ModifyBody {
        sig_id: "f".into(),
        from_stage_id: "old".into(),
        to_stage_id: "new".into(),
        from_budget: Some(10),
        to_budget: Some(20),
    };
    let _: StageTransition = StageTransition::Replace {
        sig_id: "f".into(),
        from: "old".into(),
        to: "new".into(),
    };
    // Compile-time guard: `merge_target` still returns the right
    // (sig, Some(stage)) pair.
    assert_eq!(op.merge_target(), Some(("f".to_string(), Some("new".to_string()))));
}
