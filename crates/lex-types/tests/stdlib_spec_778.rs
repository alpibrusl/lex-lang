//! #778: the declarative builtin catalogue is the type-checker's source
//! for `std.str` and `std.list`. Every signature must parse, the
//! generated module records must carry exactly the builtins the old
//! hand-written tables did, and effect rows must tie a closure's
//! effects to the call.

use lex_types::stdlib_spec::{
    declared_modules, defs_for, lookup, module_record, parse_signature, BuiltinKind, BUILTINS,
    IndexConvention,
};
use lex_types::types::{EffectSet, Ty};
use lex_types::env::TypeEnv;

#[test]
fn every_signature_parses() {
    for (i, def) in BUILTINS.iter().enumerate() {
        let ty = parse_signature(def, 1000 + i as u32)
            .unwrap_or_else(|e| panic!("{}.{}: {e}", def.module, def.name));
        assert!(matches!(ty, Ty::Function { .. }), "{}.{} is not a function", def.module, def.name);
    }
}

#[test]
fn no_duplicate_definitions() {
    let mut seen = std::collections::HashSet::new();
    for def in BUILTINS {
        assert!(seen.insert((def.module, def.name)), "duplicate {}.{}", def.module, def.name);
    }
}

#[test]
fn declared_modules_are_str_and_list() {
    assert_eq!(declared_modules(), vec!["str", "list"]);
}

#[test]
fn module_scope_is_served_from_the_catalogue() {
    let env = TypeEnv::default();
    for m in declared_modules() {
        let from_scope = lex_types::builtins::module_scope(m, &env).expect("scope");
        let from_spec = module_record(m).expect("record");
        assert_eq!(from_scope, from_spec, "{m}");
    }
}

#[test]
fn str_record_lists_every_builtin_in_order() {
    let Some(Ty::Record(fields)) = module_record("str") else { panic!("str record") };
    let names: Vec<&str> = fields.keys().map(|k| k.as_str()).collect();
    assert_eq!(names, vec![
        "is_empty", "to_int", "to_float", "concat", "len", "char_at", "split", "join",
        "starts_with", "ends_with", "contains", "cmp", "replace", "trim", "to_upper",
        "to_lower", "strip_prefix", "strip_suffix", "slice", "is_ascii", "find", "find_any",
    ]);
}

#[test]
fn list_record_lists_every_builtin_in_order() {
    let Some(Ty::Record(fields)) = module_record("list") else { panic!("list record") };
    let names: Vec<&str> = fields.keys().map(|k| k.as_str()).collect();
    assert_eq!(names, vec![
        "map", "par_map", "sort_by", "filter", "fold", "len", "is_empty", "range", "head",
        "tail", "concat", "reverse", "cons", "enumerate",
    ]);
}

#[test]
fn concrete_signatures_match_the_old_tables() {
    let Some(Ty::Record(fields)) = module_record("str") else { panic!() };
    assert_eq!(
        fields["find"],
        Ty::function(
            vec![Ty::str(), Ty::str(), Ty::int()],
            EffectSet::empty(),
            Ty::Con("Option".into(), vec![Ty::int()]),
        )
    );
    assert_eq!(
        fields["split"],
        Ty::function(vec![Ty::str(), Ty::str()], EffectSet::empty(), Ty::List(Box::new(Ty::str()))),
    );
    let Some(Ty::Record(fields)) = module_record("list") else { panic!() };
    assert_eq!(
        fields["enumerate"],
        Ty::function(
            vec![Ty::List(Box::new(Ty::Var(0)))],
            EffectSet::empty(),
            Ty::List(Box::new(Ty::Tuple(vec![Ty::int(), Ty::Var(0)]))),
        )
    );
    assert_eq!(
        fields["cons"],
        Ty::function(
            vec![Ty::Var(0), Ty::List(Box::new(Ty::Var(0)))],
            EffectSet::empty(),
            Ty::List(Box::new(Ty::Var(0))),
        )
    );
}

/// `list.map :: (List[a], (a) -> [| E] b) -> [| E] List[b]`: the closure's
/// row and the call's row are the same variable, and each higher-order
/// function gets its own.
#[test]
fn higher_order_rows_are_shared_within_and_distinct_across_builtins() {
    let Some(Ty::Record(fields)) = module_record("list") else { panic!() };
    let mut rows = Vec::new();
    for name in ["map", "par_map", "sort_by", "filter", "fold"] {
        let Ty::Function { params, effects, ret } = &fields[name] else { panic!("{name}") };
        let outer = effects.var.expect("open row on the call");
        let closure = params.iter().find_map(|p| match p {
            Ty::Function { effects, .. } => effects.var,
            _ => None,
        }).expect("closure param with a row");
        assert_eq!(outer, closure, "{name}: closure row must be the call's row");
        assert!(effects.concrete.is_empty(), "{name}: no concrete effects");
        assert!(!rows.contains(&outer), "{name}: row id {outer} reused");
        rows.push(outer);
        match name {
            "map" | "par_map" => assert_eq!(**ret, Ty::List(Box::new(Ty::Var(1)))),
            "sort_by" | "filter" => assert_eq!(**ret, Ty::List(Box::new(Ty::Var(0)))),
            "fold" => assert_eq!(**ret, Ty::Var(1)),
            _ => unreachable!(),
        }
    }
    // Type variables are numbered from 0 per builtin; row ids stay
    // clear of that range.
    for r in rows {
        assert!(r >= 100, "row id {r} collides with type-variable ids");
    }
}

#[test]
fn a_bare_bracketed_name_is_an_effect_not_a_row() {
    // `[E]` is a concrete effect named E; only `[| E]` opens a row. A
    // typo here would silently make the builtin look effectful.
    for def in BUILTINS {
        assert!(!def.ty.contains("[E]"), "{}.{}: write `[| E]` for an open row", def.module, def.name);
    }
}

#[test]
fn a_program_still_checks_against_the_catalogue() {
    let src = r#"
import "std.str" as str
import "std.list" as list

fn shout(xs :: List[Str]) -> List[Str] {
  list.map(xs, fn (s :: Str) -> Str { str.to_upper(s) })
}

fn first_pos(s :: Str) -> Option[Int] { str.find(s, ",", 0) }
"#;
    let prog = lex_syntax::parse_source(src).expect("parse");
    let stages = lex_ast::canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
}

#[test]
fn index_conventions_are_declared_for_every_positional_builtin() {
    // Every str builtin that takes or returns a position says which
    // convention it uses (the runtime test probes each one); the rest
    // declare none, so a new positional builtin cannot slip in silently.
    let positional = ["len", "char_at", "slice", "find", "find_any"];
    for def in defs_for("str") {
        if positional.contains(&def.name) {
            assert_ne!(def.index, IndexConvention::None,
                "str.{}: takes or returns a position, declare Byte or Codepoint", def.name);
        } else {
            assert_eq!(def.index, IndexConvention::None,
                "str.{}: declares an index convention but has no positions", def.name);
        }
    }
    assert_eq!(lookup("str", "len").unwrap().index, IndexConvention::Byte);
    assert_eq!(lookup("str", "slice").unwrap().index, IndexConvention::Codepoint);
    assert_eq!(lookup("list", "map").unwrap().kind, BuiltinKind::VmNative);
    assert!(lookup("str", "nope").is_none());
}
