//! #777: the checker's parse-rewrite side tables are keyed by
//! `ParseSite { stage, node }` (a stable NodeId path), not by the
//! address of the checked expression. So a `ProgramTypes` computed
//! from one copy of the stages must apply to a structurally identical
//! *other* copy — a deep clone here — exactly as it would to the
//! original.

use lex_ast::{canonicalize_program, CExpr, NodeId, Stage};
use lex_syntax::parse_source;
use lex_types::{check_and_rewrite_program, check_program, rewrite_parse_calls, ParseSite};

const SRC: &str = r#"
import "std.toml" as toml

type Manifest = { license :: Str, version :: Str }

fn plain(s :: Str) -> Result[Manifest, Str] { toml.parse(s) }

fn nested(s :: Str) -> Result[Manifest, Str] {
  let x := 1
  match x {
    1 => toml.parse(s),
    _ => Err("no"),
  }
}
"#;

fn callee_field(e: &CExpr) -> Option<&str> {
    if let CExpr::Call { callee, .. } = e {
        if let CExpr::FieldAccess { field, .. } = callee.as_ref() {
            return Some(field.as_str());
        }
    }
    None
}

fn body_of(stages: &[Stage], name: &str) -> CExpr {
    stages.iter().find_map(|s| match s {
        Stage::FnDecl(fd) if fd.name == name => Some(fd.body.clone()),
        _ => None,
    }).expect("fn present")
}

fn match_arm_body(e: &CExpr, arm: usize) -> &CExpr {
    match e {
        CExpr::Block { result, .. } => match_arm_body(result, arm),
        CExpr::Let { body, .. } => match_arm_body(body, arm),
        CExpr::Match { arms, .. } => &arms[arm].body,
        other => panic!("unexpected shape: {other:?}"),
    }
}

#[test]
fn side_tables_are_keyed_by_stage_and_node_id() {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    let pt = check_program(&stages).expect("type-check");

    // Stage 0 = import, 1 = type decl, 2 = `plain`, 3 = `nested`.
    // `plain`'s body is child 2 of the fn (one param, return type,
    // body) and *is* the call: n_0.2.
    let plain_site = ParseSite { stage: 2, node: NodeId("n_0.2".into()) };
    assert_eq!(
        pt.parse_required_fields.get(&plain_site).map(Vec::as_slice),
        Some(["license".to_string(), "version".to_string()].as_slice()),
    );
    assert_eq!(pt.parse_required_fields.len(), 2, "both parse calls recorded");
    assert!(pt.parse_required_fields.keys().all(|s| s.stage == 2 || s.stage == 3));
    assert_eq!(pt.parse_type_schemas.len(), 2);
}

#[test]
fn rewrite_applies_to_a_deep_clone_of_the_checked_stages() {
    let prog = parse_source(SRC).expect("parse");
    let original = canonicalize_program(&prog);
    let pt = check_program(&original).expect("type-check");

    // A fresh allocation: every expression address differs from the
    // ones the checker saw.
    let mut copy = original.clone();
    rewrite_parse_calls(&mut copy, &pt);

    let plain = body_of(&copy, "plain");
    assert_eq!(callee_field(&plain), Some("parse_strict_typed"));
    if let CExpr::Call { args, .. } = &plain {
        assert_eq!(args.len(), 3, "source, required fields, schema");
    }
    let nested = body_of(&copy, "nested");
    assert_eq!(callee_field(match_arm_body(&nested, 0)), Some("parse_strict_typed"));

    // The original is untouched.
    assert_eq!(callee_field(&body_of(&original, "plain")), Some("parse"));
}

#[test]
fn deep_clone_rewrite_matches_in_place_rewrite() {
    let prog = parse_source(SRC).expect("parse");
    let mut in_place = canonicalize_program(&prog);
    let mut via_clone = in_place.clone();

    check_and_rewrite_program(&mut in_place).expect("type-check");
    let pt = check_program(&via_clone.clone()).expect("type-check");
    rewrite_parse_calls(&mut via_clone, &pt);

    assert_eq!(in_place, via_clone);
}

#[test]
#[should_panic(expected = "names no expression")]
fn rewrite_against_different_stages_is_a_hard_error() {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    let pt = check_program(&stages).expect("type-check");

    // Same stage count, but `plain`'s body has been replaced by a
    // leaf: NodeId n_0.2 still exists, but the nested site's path
    // does not.
    let mut other = stages.clone();
    if let Stage::FnDecl(fd) = &mut other[3] {
        fd.body = CExpr::Literal { value: lex_ast::CLit::Int { value: 1 } };
    }
    rewrite_parse_calls(&mut other, &pt);
}

#[test]
fn programs_without_decode_imports_record_nothing() {
    let src = r#"
fn f(x :: Int) -> Int { x + 1 }
"#;
    let prog = parse_source(src).expect("parse");
    let mut stages = canonicalize_program(&prog);
    let pt = check_and_rewrite_program(&mut stages).expect("type-check");
    assert!(pt.parse_required_fields.is_empty());
    assert!(pt.parse_type_schemas.is_empty());
}
