//! M1 acceptance: round-trip parse → pretty-print → parse for §3.13 examples.

use lex_syntax::{parse_source, print_program};

fn round_trip(src: &str) {
    let prog1 = parse_source(src).unwrap_or_else(|e| panic!("parse failed: {e}\nsource:\n{src}"));
    let printed = print_program(&prog1);
    let prog2 = parse_source(&printed).unwrap_or_else(|e| {
        panic!("re-parse failed: {e}\nprinted:\n{printed}\noriginal:\n{src}")
    });
    assert_eq!(prog1, prog2, "round trip differs.\nprinted:\n{printed}");
}

#[test]
fn example_a_factorial() {
    round_trip(include_str!("../../../examples/a_factorial.lex"));
}

#[test]
fn example_b_parse_int() {
    round_trip(include_str!("../../../examples/b_parse_int.lex"));
}

#[test]
fn example_c_echo() {
    round_trip(include_str!("../../../examples/c_echo.lex"));
}

#[test]
fn example_d_shape() {
    round_trip(include_str!("../../../examples/d_shape.lex"));
}

#[test]
fn parses_simple_let() {
    round_trip("fn id(x :: Int) -> Int {\n  let y :: Int := x\n  y\n}\n");
}

#[test]
fn parses_pipe() {
    round_trip("fn f(x :: Int) -> Int { x |> g |> h }\n");
}

#[test]
fn parses_record_literal() {
    round_trip("fn p() -> Int { let r := { x: 1, y: 2 }\n r.x }\n");
}
