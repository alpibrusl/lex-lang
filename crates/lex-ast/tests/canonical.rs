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
