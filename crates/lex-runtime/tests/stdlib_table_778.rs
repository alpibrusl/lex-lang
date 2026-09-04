//! #778: the runtime's implementation table for the declared stdlib
//! modules must match the catalogue in `lex_types::stdlib_spec` one to
//! one, the by-value and borrowed entry points must agree, and every
//! declared index convention must hold at runtime.

use lex_bytecode::Value;
use lex_runtime::{call_pure_builtin, is_pure_call, try_pure_builtin};
use lex_types::stdlib_spec::{defs_for, declared_modules, lookup, BuiltinKind, IndexConvention};

fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn some(v: Value) -> Value {
    Value::Variant { name: "Some".into(), args: vec![v] }
}
fn none() -> Value {
    Value::Variant { name: "None".into(), args: vec![] }
}
fn list(xs: Vec<Value>) -> Value {
    Value::List(xs.into_iter().collect::<std::collections::VecDeque<_>>().into())
}
fn call(module: &str, name: &str, args: Vec<Value>) -> Value {
    call_pure_builtin(module, name, args).unwrap_or_else(|e| panic!("{module}.{name}: {e}"))
}

/// Every `Pure` definition dispatches; every `VmNative` one is pure but
/// has no table entry; nothing outside the catalogue answers for a
/// declared module.
#[test]
fn table_matches_catalogue() {
    for m in declared_modules() {
        for d in defs_for(m) {
            match d.kind {
                BuiltinKind::Pure => {
                    // A wrong-arity call still proves the entry exists:
                    // the table answers with the builtin's own error,
                    // never the legacy "unknown" fallthrough.
                    let r = try_pure_builtin(d.module, d.name, &[]).expect("declared pure builtin is dispatchable");
                    if let Err(e) = r {
                        assert!(!e.contains("unknown"), "{}.{}: fell through to legacy dispatch: {e}", d.module, d.name);
                    }
                    assert!(is_pure_call(d.module, d.name));
                }
                BuiltinKind::VmNative => {
                    assert!(is_pure_call(d.module, d.name), "{}.{} is pure", d.module, d.name);
                    let r = try_pure_builtin(d.module, d.name, &[]).expect("pure");
                    assert!(matches!(r, Err(ref e) if e.contains("unknown")),
                        "{}.{}: VmNative must not have a table entry, got {r:?}", d.module, d.name);
                }
                BuiltinKind::Effect => panic!("{}.{}: no effectful builtins are declared yet", d.module, d.name),
            }
        }
    }
    // An undeclared name in a declared module is not silently pure.
    assert!(lookup("str", "shout").is_none());
    assert!(matches!(try_pure_builtin("str", "shout", &[]), Some(Err(_))));
}

#[test]
fn borrowed_and_owned_entry_points_agree() {
    let xs = list(vec![Value::Int(2), Value::Int(3)]);
    let owned = call("list", "cons", vec![Value::Int(1), xs.clone()]);
    let borrowed = try_pure_builtin("list", "cons", &[Value::Int(1), xs.clone()]).unwrap().unwrap();
    assert_eq!(owned, borrowed);
    assert_eq!(owned, list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]));
    // The input is untouched on both paths (copy-on-write).
    assert_eq!(xs, list(vec![Value::Int(2), Value::Int(3)]));

    let t_owned = call("list", "tail", vec![xs.clone()]);
    let t_borrowed = try_pure_builtin("list", "tail", &[xs.clone()]).unwrap().unwrap();
    assert_eq!(t_owned, t_borrowed);
    assert_eq!(t_owned, list(vec![Value::Int(3)]));
    assert_eq!(call("list", "tail", vec![list(vec![])]), list(vec![]));
    assert_eq!(call("list", "cons", vec![Value::Int(1)]), list(vec![Value::Int(1)]));
    assert_eq!(call("list", "reverse", vec![xs.clone()]), list(vec![Value::Int(3), Value::Int(2)]));
    assert_eq!(
        call("list", "concat", vec![xs.clone(), list(vec![Value::Int(4)])]),
        list(vec![Value::Int(2), Value::Int(3), Value::Int(4)])
    );
}

/// "héllo": `é` is two bytes, one codepoint. Each str builtin with a
/// declared convention is probed here; a new positional builtin fails
/// this test until it gets a probe.
#[test]
fn declared_index_conventions_hold() {
    let probes: &[(&str, IndexConvention, Vec<Value>, Value)] = &[
        ("len", IndexConvention::Byte, vec![s("héllo")], Value::Int(6)),
        ("char_at", IndexConvention::Byte, vec![s("héllo"), Value::Int(0)], s("h")),
        ("char_at", IndexConvention::Byte, vec![s("héllo"), Value::Int(1)], s("")),
        ("char_at", IndexConvention::Byte, vec![s("héllo"), Value::Int(3)], s("l")),
        ("slice", IndexConvention::Codepoint, vec![s("héllo"), Value::Int(1), Value::Int(2)], s("é")),
        ("slice", IndexConvention::Codepoint, vec![s("héllo"), Value::Int(0), Value::Int(99)], s("héllo")),
        ("find", IndexConvention::Codepoint, vec![s("héllo"), s("l"), Value::Int(0)], some(Value::Int(2))),
        ("find", IndexConvention::Codepoint, vec![s("héllo"), s("z"), Value::Int(0)], none()),
        ("find_any", IndexConvention::Codepoint, vec![s("héllo"), s("ol"), Value::Int(3)], some(Value::Int(3))),
    ];
    let mut probed = std::collections::HashSet::new();
    for (name, conv, args, want) in probes {
        let def = lookup("str", name).unwrap_or_else(|| panic!("str.{name} not declared"));
        assert_eq!(def.index, *conv, "str.{name}: probe convention must match the declaration");
        assert_eq!(&call("str", name, args.clone()), want, "str.{name}({args:?})");
        probed.insert(*name);
    }
    for d in defs_for("str") {
        if d.index != IndexConvention::None {
            assert!(probed.contains(d.name), "str.{}: declares {:?} but has no probe", d.name, d.index);
        }
    }
}

#[test]
fn every_str_builtin_behaves_as_before() {
    assert_eq!(call("str", "is_empty", vec![s("")]), Value::Bool(true));
    assert_eq!(call("str", "to_int", vec![s("-42")]), some(Value::Int(-42)));
    assert_eq!(call("str", "to_int", vec![s("4x")]), none());
    assert_eq!(call("str", "to_float", vec![s("2.5")]), some(Value::Float(2.5)));
    assert_eq!(call("str", "concat", vec![s("a"), s("b")]), s("ab"));
    assert_eq!(call("str", "split", vec![s("a,b"), s(",")]), list(vec![s("a"), s("b")]));
    assert_eq!(call("str", "split", vec![s("ab"), s("")]), list(vec![s("a"), s("b")]));
    assert_eq!(call("str", "join", vec![list(vec![s("a"), s("b")]), s("-")]), s("a-b"));
    assert!(call_pure_builtin("str", "join", vec![list(vec![Value::Int(1)]), s("-")]).is_err());
    assert_eq!(call("str", "starts_with", vec![s("abc"), s("ab")]), Value::Bool(true));
    assert_eq!(call("str", "ends_with", vec![s("abc"), s("bc")]), Value::Bool(true));
    assert_eq!(call("str", "contains", vec![s("abc"), s("b")]), Value::Bool(true));
    assert_eq!(call("str", "cmp", vec![s("a"), s("b")]), Value::Int(-1));
    assert_eq!(call("str", "replace", vec![s("aXa"), s("X"), s("-")]), s("a-a"));
    assert_eq!(call("str", "trim", vec![s("  x ")]), s("x"));
    assert_eq!(call("str", "to_upper", vec![s("é")]), s("É"));
    assert_eq!(call("str", "to_lower", vec![s("É")]), s("é"));
    assert_eq!(call("str", "strip_prefix", vec![s("abc"), s("a")]), some(s("bc")));
    assert_eq!(call("str", "strip_suffix", vec![s("abc"), s("x")]), none());
    assert!(call_pure_builtin("str", "slice", vec![s("abc"), Value::Int(2), Value::Int(1)]).is_err());
    assert_eq!(call("str", "is_ascii", vec![s("héllo")]), Value::Bool(false));
    assert_eq!(call("str", "find", vec![s("abc"), s(""), Value::Int(3)]), some(Value::Int(3)));
    assert_eq!(call("list", "len", vec![list(vec![Value::Int(1)])]), Value::Int(1));
    assert_eq!(call("list", "is_empty", vec![list(vec![])]), Value::Bool(true));
    assert_eq!(call("list", "head", vec![list(vec![Value::Int(7)])]), some(Value::Int(7)));
    assert_eq!(call("list", "head", vec![list(vec![])]), none());
    assert_eq!(call("list", "range", vec![Value::Int(1), Value::Int(3)]), list(vec![Value::Int(1), Value::Int(2)]));
    assert_eq!(
        call("list", "enumerate", vec![list(vec![s("a")])]),
        list(vec![Value::Tuple(vec![Value::Int(0), s("a")])])
    );
}

#[test]
fn purity_answers_are_unchanged_for_undeclared_modules() {
    assert!(is_pure_call("int", "to_str"));
    assert!(!is_pure_call("crypto", "random"));
    assert!(!is_pure_call("net", "get"));
    // An undeclared name in a declared module keeps the module-level
    // answer (the checker rejects it long before dispatch, which then
    // reports it as unknown), as it always did.
    assert!(lookup("str", "no_such_builtin").is_none());
    assert!(is_pure_call("str", "no_such_builtin"));
}
