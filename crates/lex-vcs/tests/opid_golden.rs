//! Golden hashes for the V1 canonical form.
//!
//! Each entry in [`GOLDENS`] pins the canonical pre-image bytes (what
//! gets hashed to derive `op_id`) and the resulting `OpId` for one
//! representative operation. **Updating any pinned value rewrites
//! every `OpId` in every existing store** and must be a deliberate,
//! versioned change (see issue #244).
//!
//! Coverage targets every `OperationKind` variant plus the two
//! orthogonal axes that affect the hash but not the kind shape:
//!
//! - parameterized effects (`net("wttr.in")`-style strings inside
//!   the `EffectSet`)
//! - intent linkage (`Operation::with_intent`)
//!
//! The harness asserts byte-for-byte equality on the pre-image *and*
//! on the SHA-256 digest, so a regression in `serde_json` field
//! ordering, a `BTreeMap` → `HashMap` swap, or an enum-variant
//! reorder all surface as a hard test failure.

use lex_vcs::{Operation, OperationKind};
use std::collections::BTreeSet;

/// One pinned operation.
struct Golden {
    name: &'static str,
    op: fn() -> Operation,
    /// The exact bytes fed to SHA-256, as a UTF-8 JSON string. Pinning
    /// the JSON (not the bytes hex) keeps the source readable; the
    /// canonical form is ASCII-clean by design.
    canonical_json: &'static str,
    op_id: &'static str,
}

fn empty_effects() -> BTreeSet<String> {
    BTreeSet::new()
}

fn add_function() -> Operation {
    Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac::Int->Int".into(),
            stage_id: "abc123".into(),
            effects: empty_effects(),
            budget_cost: None,
        },
        [],
    )
}

fn remove_function() -> Operation {
    Operation::new(
        OperationKind::RemoveFunction {
            sig_id: "fac::Int->Int".into(),
            last_stage_id: "abc123".into(),
        },
        ["op-parent".into()],
    )
}

fn modify_body() -> Operation {
    Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac::Int->Int".into(),
            from_stage_id: "abc123".into(),
            to_stage_id: "def456".into(),
            from_budget: None,
            to_budget: None,
        },
        ["op-parent".into()],
    )
}

fn rename_symbol() -> Operation {
    Operation::new(
        OperationKind::RenameSymbol {
            from: "parse::Str->Int".into(),
            to: "parse_int::Str->Int".into(),
            body_stage_id: "abc123".into(),
        },
        ["op-parent".into()],
    )
}

fn change_effect_sig() -> Operation {
    let from: BTreeSet<String> = BTreeSet::new();
    let to: BTreeSet<String> = ["io".into()].into_iter().collect();
    Operation::new(
        OperationKind::ChangeEffectSig {
            sig_id: "writer::Str->()".into(),
            from_stage_id: "old".into(),
            to_stage_id: "new".into(),
            from_effects: from,
            to_effects: to,
            from_budget: None,
            to_budget: None,
        },
        ["op-parent".into()],
    )
}

fn add_import() -> Operation {
    Operation::new(
        OperationKind::AddImport {
            in_file: "src/main.lex".into(),
            module: "std.io".into(),
        },
        ["op-parent".into()],
    )
}

fn add_type() -> Operation {
    Operation::new(
        OperationKind::AddType {
            sig_id: "Color".into(),
            stage_id: "type-stage-1".into(),
        },
        [],
    )
}

fn merge_two_parents() -> Operation {
    Operation::new(
        OperationKind::Merge { resolved: 3 },
        ["op-a".into(), "op-b".into()],
    )
}

fn add_function_with_intent() -> Operation {
    Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac::Int->Int".into(),
            stage_id: "abc123".into(),
            effects: empty_effects(),
            budget_cost: None,
        },
        [],
    )
    .with_intent("intent-a")
}

fn add_function_parameterized_effect() -> Operation {
    // EffectSet is BTreeSet<String> in lex-vcs; the type-system
    // pretty form `net("wttr.in")` is preserved verbatim as the
    // string identity. Two ops with the same parameterized form
    // hash equal; a bare `net` and a parameterized `net("wttr.in")`
    // hash distinctly — that's what makes per-capability effect
    // parameterization (#207) load-bearing in the op log.
    let effects: BTreeSet<String> = ["net(\"wttr.in\")".into()].into_iter().collect();
    Operation::new(
        OperationKind::AddFunction {
            sig_id: "fetch_weather::Str->Str".into(),
            stage_id: "stage-w-1".into(),
            effects,
            budget_cost: None,
        },
        ["op-parent".into()],
    )
}

const GOLDENS: &[Golden] = &[
    Golden {
        name: "add_function",
        op: add_function,
        canonical_json: r#"{"op":"add_function","sig_id":"fac::Int->Int","stage_id":"abc123","effects":[],"parents":[]}"#,
        op_id: "f112990d31ef2a63f3e5ca5680637ed36a54bc7e8230510ae0c0e93fcb39d104",
    },
    Golden {
        name: "remove_function",
        op: remove_function,
        canonical_json: r#"{"op":"remove_function","sig_id":"fac::Int->Int","last_stage_id":"abc123","parents":["op-parent"]}"#,
        op_id: "32cfe555d2fcc1a687c2660ab1f7a8c7f0016bad4f5bbc2cf84b7901559d5d54",
    },
    Golden {
        name: "modify_body",
        op: modify_body,
        canonical_json: r#"{"op":"modify_body","sig_id":"fac::Int->Int","from_stage_id":"abc123","to_stage_id":"def456","parents":["op-parent"]}"#,
        op_id: "feaf802dbe4fdd252b4d6608aa73aab3690a7a1518078e8be6dc46e5149ab9c9",
    },
    Golden {
        name: "rename_symbol",
        op: rename_symbol,
        canonical_json: r#"{"op":"rename_symbol","from":"parse::Str->Int","to":"parse_int::Str->Int","body_stage_id":"abc123","parents":["op-parent"]}"#,
        op_id: "30bb996d53224c62a9548c2e2c9064222954dfd8f4428be3908c96e6ea46053f",
    },
    Golden {
        name: "change_effect_sig",
        op: change_effect_sig,
        canonical_json: r#"{"op":"change_effect_sig","sig_id":"writer::Str->()","from_stage_id":"old","to_stage_id":"new","from_effects":[],"to_effects":["io"],"parents":["op-parent"]}"#,
        op_id: "bbd95c21a074bfa92c83b24739c3855db0d54fcd32c0e14aa52699e1c4086c16",
    },
    Golden {
        name: "add_import",
        op: add_import,
        canonical_json: r#"{"op":"add_import","in_file":"src/main.lex","module":"std.io","parents":["op-parent"]}"#,
        op_id: "1c13ef99e244a7e2a150135a83b49cf9c848314a507560105f4fc1f8b3870618",
    },
    Golden {
        name: "add_type",
        op: add_type,
        canonical_json: r#"{"op":"add_type","sig_id":"Color","stage_id":"type-stage-1","parents":[]}"#,
        op_id: "567c9f91acfddda0af5660ef2adc7de6cb3cb8b81903e6db5053c330295bb67e",
    },
    Golden {
        name: "merge_two_parents",
        op: merge_two_parents,
        canonical_json: r#"{"op":"merge","resolved":3,"parents":["op-a","op-b"]}"#,
        op_id: "24a93e8e524eee4fc4b7690f92a1776457af0560443df6f9db6e9c2cffec0f85",
    },
    Golden {
        name: "add_function_with_intent",
        op: add_function_with_intent,
        canonical_json: r#"{"op":"add_function","sig_id":"fac::Int->Int","stage_id":"abc123","effects":[],"parents":[],"intent_id":"intent-a"}"#,
        op_id: "e5a2e81c186abb83dd9e033d2b0504b0dc678df5ce2b38f8d2b33f6f2d7ae219",
    },
    Golden {
        name: "add_function_parameterized_effect",
        op: add_function_parameterized_effect,
        canonical_json: r#"{"op":"add_function","sig_id":"fetch_weather::Str->Str","stage_id":"stage-w-1","effects":["net(\"wttr.in\")"],"parents":["op-parent"]}"#,
        op_id: "3bfc418d9ff7b334eb784fa103216f23ed4e91a51c4ed7cc193cb6592ebea23f",
    },
];

/// Helper used during golden capture: print the live canonical pre-
/// image and `op_id` for every entry. Run with `cargo test -p
/// lex-vcs --test opid_golden -- --ignored capture --nocapture`.
#[test]
#[ignore]
fn capture() {
    for g in GOLDENS {
        let op = (g.op)();
        let bytes = op.canonical_bytes();
        let json = std::str::from_utf8(&bytes).expect("canonical form is utf-8");
        println!("{}:", g.name);
        println!("  canonical_json: {json}");
        println!("  op_id:          {}", op.op_id());
    }
}

#[test]
fn golden_op_ids_are_stable() {
    for g in GOLDENS {
        let op = (g.op)();
        let live = op.op_id();
        assert_eq!(
            live, g.op_id,
            "op_id drift on `{}`: every existing op_id has rotated. \
             If this change is intentional, bump the operation \
             format version (issue #244) and update this golden.",
            g.name,
        );
    }
}

#[test]
fn golden_canonical_bytes_are_stable() {
    for g in GOLDENS {
        let op = (g.op)();
        let live_bytes = op.canonical_bytes();
        let live_json = std::str::from_utf8(&live_bytes).expect("canonical form is utf-8");
        assert_eq!(
            live_json, g.canonical_json,
            "canonical-form drift on `{}`: pre-image changed, so \
             every existing op_id has rotated. If intentional, bump \
             the operation format version (issue #244) and update \
             this golden.",
            g.name,
        );
    }
}

#[test]
fn op_id_is_sha256_of_canonical_bytes() {
    use sha2::{Digest, Sha256};
    for g in GOLDENS {
        let op = (g.op)();
        let bytes = op.canonical_bytes();
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, g.op_id,
            "`{}`: op_id is not SHA-256 of canonical_bytes — the \
             contract between Operation::canonical_bytes and \
             Operation::op_id has drifted.",
            g.name,
        );
    }
}

#[test]
fn every_variant_is_covered() {
    // If `OperationKind` gains a variant, this fails until a golden
    // is added — every variant must have a pinned canonical form so
    // a structural break is loud, not silent. RemoveImport,
    // RemoveType, and ModifyType share the canonical *structure* of
    // their counterparts (AddImport, AddType, ModifyBody) and are
    // exercised by the property suite; they are not mandatory in
    // this golden table.
    let covered: std::collections::BTreeSet<&str> = GOLDENS
        .iter()
        .map(|g| variant_tag(&(g.op)().kind))
        .collect();

    let required: &[&str] = &[
        "add_function",
        "remove_function",
        "modify_body",
        "rename_symbol",
        "change_effect_sig",
        "add_import",
        "add_type",
        "merge",
    ];
    for tag in required {
        assert!(
            covered.contains(tag),
            "variant `{tag}` is not pinned in opid_golden::GOLDENS",
        );
    }
}

fn variant_tag(k: &OperationKind) -> &'static str {
    match k {
        OperationKind::AddFunction { .. } => "add_function",
        OperationKind::RemoveFunction { .. } => "remove_function",
        OperationKind::ModifyBody { .. } => "modify_body",
        OperationKind::RenameSymbol { .. } => "rename_symbol",
        OperationKind::ChangeEffectSig { .. } => "change_effect_sig",
        OperationKind::AddImport { .. } => "add_import",
        OperationKind::RemoveImport { .. } => "remove_import",
        OperationKind::AddType { .. } => "add_type",
        OperationKind::RemoveType { .. } => "remove_type",
        OperationKind::ModifyType { .. } => "modify_type",
        OperationKind::Merge { .. } => "merge",
    }
}
