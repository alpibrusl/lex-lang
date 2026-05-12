//! Acceptance tests for #363 slice 1: `{ ...TypeName }` record spread syntax.
//!
//! Scope: spread type expressions parse, canonicalize, and are resolved to
//! flat `Ty::Record` during type-checking. Functions accepting or returning
//! spread types unify correctly with concrete record literals.

use lex_ast::canonicalize_program;
use lex_ast::TypeExpr;
use lex_syntax::parse_source;
use lex_types::check_program;

fn canon_stages(src: &str) -> Vec<lex_ast::Stage> {
    let p = parse_source(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
    canonicalize_program(&p)
}

#[test]
fn spread_type_parses_and_canonicalizes() {
    let src = r#"
type Post = { title :: Str, body :: Str }
type Tagged = { ...Post, tag :: Str }
"#;
    let stages = canon_stages(src);
    let tagged_decl = stages.iter().find_map(|s| match s {
        lex_ast::Stage::TypeDecl(td) if td.name == "Tagged" => Some(td),
        _ => None,
    }).expect("Tagged type decl");
    match &tagged_decl.definition {
        TypeExpr::RecordWithSpreads { spreads, fields } => {
            assert_eq!(spreads, &["Post"]);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "tag");
        }
        other => panic!("expected RecordWithSpreads, got {other:?}"),
    }
}

#[test]
fn spread_type_resolves_during_type_check() {
    let src = r#"
type Post = { title :: Str, body :: Str }
type Tagged = { ...Post, tag :: Str }

fn make_tagged(title :: Str, body :: Str, tag :: Str) -> Tagged {
    { title: title, body: body, tag: tag }
}
"#;
    let stages = canon_stages(src);
    check_program(&stages).expect("type check should pass");
}

#[test]
fn spread_type_with_multiple_spreads_parses() {
    let src = r#"
type HasName = { name :: Str }
type HasAge = { age :: Int }
type Person = { ...HasName, ...HasAge }
"#;
    let stages = canon_stages(src);
    let person_decl = stages.iter().find_map(|s| match s {
        lex_ast::Stage::TypeDecl(td) if td.name == "Person" => Some(td),
        _ => None,
    }).expect("Person type decl");
    match &person_decl.definition {
        TypeExpr::RecordWithSpreads { spreads, fields } => {
            assert_eq!(spreads.len(), 2);
            assert!(spreads.contains(&"HasName".to_string()));
            assert!(spreads.contains(&"HasAge".to_string()));
            assert!(fields.is_empty());
        }
        other => panic!("expected RecordWithSpreads, got {other:?}"),
    }
}

#[test]
fn spread_only_without_extra_fields_parses() {
    let src = r#"
type Base = { x :: Int, y :: Int }
type Derived = { ...Base }

fn use_derived(d :: Derived) -> Int { d.x }
"#;
    let stages = canon_stages(src);
    check_program(&stages).expect("type check should pass");
}
