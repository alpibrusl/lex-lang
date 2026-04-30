//! M4 acceptance: pure §3.13 examples produce expected outputs.

use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

#[test]
fn example_a_factorial() {
    let src = include_str!("../../../examples/a_factorial.lex");
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("factorial", vec![Value::Int(5)]).unwrap();
    assert_eq!(r, Value::Int(120));
    let r = vm.call("factorial", vec![Value::Int(0)]).unwrap();
    assert_eq!(r, Value::Int(1));
    let r = vm.call("factorial", vec![Value::Int(10)]).unwrap();
    assert_eq!(r, Value::Int(3628800));
}

#[test]
fn example_d_shape() {
    let src = include_str!("../../../examples/d_shape.lex");
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let circle = Value::Variant {
        name: "Circle".into(),
        args: vec![Value::Record({
            let mut m = IndexMap::new();
            m.insert("radius".into(), Value::Float(1.0));
            m
        })],
    };
    let r = vm.call("area", vec![circle]).unwrap();
    let v = match r { Value::Float(f) => f, other => panic!("expected float, got {other:?}") };
    // Source uses 3.14159 directly (the spec's example, not std::f64::consts::PI).
    #[allow(clippy::approx_constant)]
    let expected_area = 3.14159_f64;
    assert!((v - expected_area).abs() < 1e-6, "got {v}");

    let rect = Value::Variant {
        name: "Rect".into(),
        args: vec![Value::Record({
            let mut m = IndexMap::new();
            m.insert("width".into(), Value::Float(2.0));
            m.insert("height".into(), Value::Float(3.0));
            m
        })],
    };
    let r = vm.call("area", vec![rect]).unwrap();
    assert_eq!(r, Value::Float(6.0));
}

#[test]
fn bytecode_is_reproducible() {
    let src = include_str!("../../../examples/a_factorial.lex");
    let p1 = compile(src);
    let p2 = compile(src);
    assert_eq!(p1, p2);
}

#[test]
fn match_with_literal_int() {
    let src = "fn id_or_zero(n :: Int) -> Int {\n  match n {\n    0 => 0,\n    _ => n,\n  }\n}\n";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("id_or_zero", vec![Value::Int(0)]).unwrap(), Value::Int(0));
    assert_eq!(vm.call("id_or_zero", vec![Value::Int(7)]).unwrap(), Value::Int(7));
}

#[test]
fn record_field_access() {
    let src = "fn xof(r :: Record) -> Int { r.x }\n".replace(
        "Record",
        "{ x :: Int, y :: Int }",
    );
    let p = compile(&src);
    let mut vm = Vm::new(&p);
    let mut m = IndexMap::new();
    m.insert("x".into(), Value::Int(11));
    m.insert("y".into(), Value::Int(22));
    let r = vm.call("xof", vec![Value::Record(m)]).unwrap();
    assert_eq!(r, Value::Int(11));
}
