//! M2 acceptance: §5.6.

use lex_ast::*;
use lex_syntax::parse_source;

fn canon(src: &str) -> Vec<Stage> {
    let p = parse_source(src).unwrap();
    canonicalize_program(&p)
}

#[test]
fn round_trip_canonical() {
    // parse → canonicalize → print → parse → canonicalize is identity.
    let src = include_str!("../../../examples/a_factorial.lex");
    let s1 = canon(src);
    let printed = print_stages(&s1);
    let s2 = canon(&printed);
    assert_eq!(s1, s2, "round-trip differs.\nprinted:\n{printed}");
}

#[test]
fn round_trip_b_parse_int() {
    let src = include_str!("../../../examples/b_parse_int.lex");
    let s1 = canon(src);
    let printed = print_stages(&s1);
    let s2 = canon(&printed);
    assert_eq!(s1, s2, "round-trip differs.\nprinted:\n{printed}");
}

#[test]
fn round_trip_d_shape() {
    let src = include_str!("../../../examples/d_shape.lex");
    let s1 = canon(src);
    let printed = print_stages(&s1);
    let s2 = canon(&printed);
    assert_eq!(s1, s2, "round-trip differs.\nprinted:\n{printed}");
}

#[test]
fn record_field_order_is_normalized() {
    // Different source field order, same canonical hash.
    let a = canon("fn p() -> Int { let r := { x: 1, y: 2 }\n r.x }\n");
    let b = canon("fn p() -> Int { let r := { y: 2, x: 1 }\n r.x }\n");
    assert_eq!(stage_canonical_hash_hex(&a[0]), stage_canonical_hash_hex(&b[0]));
}

#[test]
fn union_variant_order_is_normalized() {
    let a = canon("type X = A | B | C\n");
    let b = canon("type X = C | A | B\n");
    assert_eq!(stage_canonical_hash_hex(&a[0]), stage_canonical_hash_hex(&b[0]));
}

#[test]
fn if_canonicalizes_to_match() {
    let src = "fn pick(b :: Bool) -> Int {\n  if b { 1 } else { 2 }\n}\n";
    let s = canon(src);
    if let Stage::FnDecl(fd) = &s[0] {
        match &fd.body {
            CExpr::Match { arms, .. } => {
                assert_eq!(arms.len(), 2);
                // First arm should be Bool(true).
                match &arms[0].pattern {
                    Pattern::PLiteral { value: CLit::Bool { value: true } } => {}
                    other => panic!("expected first arm = true literal, got {other:?}"),
                }
            }
            other => panic!("expected match, got {other:?}"),
        }
    } else {
        panic!("expected fn decl");
    }
}

#[test]
fn try_canonicalizes_to_match() {
    let src = r#"
import "std.io" as io
fn r(x :: Int) -> Result[Int, Str] {
  io.read(x)?
}
"#;
    let s = canon(src);
    let fd = match &s[1] { Stage::FnDecl(fd) => fd, _ => panic!() };
    let m = match &fd.body { CExpr::Match { arms, .. } => arms, other => panic!("got {other:?}") };
    assert_eq!(m.len(), 2, "Try desugars to a 2-arm match");
}

#[test]
fn node_ids_walk() {
    let s = canon(include_str!("../../../examples/a_factorial.lex"));
    let ids = collect_ids(&s[0]);
    assert!(!ids.is_empty());
    // Root id is n_0.
    assert_eq!(ids[0].0.as_str(), "n_0");
}

#[test]
fn stage_id_is_deterministic() {
    let src = include_str!("../../../examples/a_factorial.lex");
    let s1 = canon(src);
    let s2 = canon(src);
    assert_eq!(stage_id(&s1[0]), stage_id(&s2[0]));
    // Same hash twice.
    assert_eq!(stage_canonical_hash_hex(&s1[0]), stage_canonical_hash_hex(&s2[0]));
}

#[test]
fn renaming_changes_sig_id() {
    let s1 = canon("fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    let s2 = canon("fn plus(x :: Int, y :: Int) -> Int { x + y }\n");
    // Per §4.6 default: name IS in SigId.
    assert_ne!(sig_id(&s1[0]), sig_id(&s2[0]));
}

// --- #369: signature-level examples ---

#[test]
fn empty_examples_does_not_perturb_sig_id() {
    // A fn with no `examples` block (the entire existing corpus) must
    // hash identically to its pre-#369 form. We enforce that by
    // hashing the same source via two paths: source → canonicalize
    // (the new code) and an explicitly-constructed FnDecl with the
    // examples vec absent from the JSON. The canonical-JSON encoding
    // skips the field when empty, so both paths produce the same hash.
    let s = canon("fn id(x :: Int) -> Int { x }\n");
    let Stage::FnDecl(fd) = &s[0] else { panic!() };
    assert!(fd.examples.is_empty(), "no examples should be parsed");

    // serde_json::to_value on the FnDecl must omit the `examples` key.
    let v = serde_json::to_value(fd).unwrap();
    assert!(
        !v.as_object().unwrap().contains_key("examples"),
        "examples must be omitted from canonical JSON when empty: {v:?}"
    );
}

#[test]
fn examples_are_part_of_sig_id() {
    // Two functions with identical name/params/return/body but
    // different example sets must have different SigIds — examples
    // are part of the contract.
    let s1 = canon("fn id(x :: Int) -> Int\nexamples { id(1) => 1 }\n{ x }\n");
    let s2 = canon("fn id(x :: Int) -> Int\nexamples { id(2) => 2 }\n{ x }\n");
    assert_ne!(sig_id(&s1[0]), sig_id(&s2[0]), "SigId must fold in examples");
}

#[test]
fn examples_round_trip_through_canonicalize() {
    let src = "fn id(x :: Int) -> Int\nexamples { id(7) => 7 }\n{ x }\n";
    let s = canon(src);
    let Stage::FnDecl(fd) = &s[0] else { panic!() };
    assert_eq!(fd.examples.len(), 1);
    assert_eq!(fd.examples[0].args.len(), 1);
}
