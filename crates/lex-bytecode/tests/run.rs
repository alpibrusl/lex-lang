//! M4 acceptance: pure §3.13 examples produce expected outputs.

use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm, VmError, MAX_CALL_DEPTH};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

#[test]
fn unbounded_recursion_yields_call_stack_overflow_not_segfault() {
    // Non-tail recursion (the `+ 1` forces the call to return before
    // we can use its result), so each call pushes a fresh frame.
    // Pre-fix the VM would push frames until the host's native stack
    // exploded; post-fix we get a clean `CallStackOverflow` once we
    // hit `MAX_CALL_DEPTH`.
    //
    // Run on a thread with a small stack so a regression (a recursion
    // path that bypasses `push_frame`) shows up as a SIGSEGV rather
    // than passing because the host stack happens to be 8 MiB.
    let src = "fn deep() -> Int { 1 + deep() }\n";
    let p = compile(src);
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut vm = Vm::new(&p);
            vm.call("deep", vec![])
        })
        .expect("spawn worker thread");
    let r = handle.join().expect("worker panicked").expect_err("expected overflow");
    match r {
        VmError::CallStackOverflow(n) => assert_eq!(n, MAX_CALL_DEPTH),
        other => panic!("expected CallStackOverflow, got {other:?}"),
    }
}

#[test]
fn modest_recursion_under_cap_still_runs() {
    // factorial(20) recurses 20 frames — well under MAX_CALL_DEPTH.
    // Sanity check that the gate doesn't reject legitimate code.
    let src = "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("factorial", vec![Value::Int(20)]).unwrap();
    assert_eq!(r, Value::Int(2_432_902_008_176_640_000));
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
