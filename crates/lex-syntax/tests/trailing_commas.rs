//! Trailing commas in every comma-separated context (closes #80).
//!
//! Before this fix, `match` arms / list literals / record literals
//! accepted a trailing comma but `fn` parameter lists / call argument
//! lists / lambda params / type args / effects / tuple patterns
//! rejected them. Adding a parameter mid-list and forgetting to remove
//! the comma you just added is a small but recurring papercut.

use lex_syntax::parse_source;

fn assert_parses(src: &str) {
    parse_source(src).unwrap_or_else(|e| panic!("expected to parse, got {e}\n--- src ---\n{src}"));
}

#[test]
fn fn_parameter_list_allows_trailing_comma() {
    assert_parses("fn add(x :: Int, y :: Int,) -> Int { x + y }\n");
}

#[test]
fn fn_call_argument_list_allows_trailing_comma() {
    assert_parses("fn main() -> Int { add(2, 3,) }\n");
}

#[test]
fn lambda_param_list_allows_trailing_comma() {
    assert_parses(
        "fn main() -> (Int) -> Int { fn (x :: Int,) -> Int { x } }\n",
    );
}

#[test]
fn effect_list_allows_trailing_comma() {
    assert_parses("fn f(x :: Int) -> [io, net,] Int { x }\n");
}

#[test]
fn type_args_allow_trailing_comma() {
    assert_parses("fn f(r :: Result[Int, Str,]) -> Int { 0 }\n");
}

#[test]
fn type_decl_params_allow_trailing_comma() {
    assert_parses("type Pair[A, B,] = { fst :: A, snd :: B }\n");
}

#[test]
fn function_type_params_allow_trailing_comma() {
    assert_parses("fn apply(f :: (Int, Int,) -> Int) -> Int { f(1, 2) }\n");
}

#[test]
fn constructor_type_payload_allows_trailing_comma() {
    assert_parses("type T = Pair(Int, Str,)\n");
}

#[test]
fn tuple_pattern_allows_trailing_comma() {
    assert_parses(
        "fn main() -> Int { match (1, 2) { (a, b,) => a + b } }\n",
    );
}

#[test]
fn constructor_pattern_args_allow_trailing_comma() {
    assert_parses(
        "type T = Pair(Int, Int)
fn main() -> Int {
  match Pair(1, 2) {
    Pair(a, b,) => a + b,
  }
}
",
    );
}

#[test]
fn list_literal_allows_trailing_comma_unchanged() {
    // Already worked before; documenting the regression boundary.
    assert_parses("fn main() -> List[Int] { [1, 2, 3,] }\n");
}

#[test]
fn record_literal_allows_trailing_comma_unchanged() {
    // Already worked before; documenting the regression boundary.
    assert_parses("fn main() -> { a :: Int, b :: Int } { { a: 1, b: 2, } }\n");
}

#[test]
fn match_arms_allow_trailing_comma_unchanged() {
    // Already worked before; documenting the regression boundary.
    assert_parses(
        r#"fn main() -> Str {
  match 0 {
    0 => "zero",
    _ => "other",
  }
}
"#,
    );
}

#[test]
fn empty_list_with_no_trailing_comma_still_works() {
    assert_parses("fn main() -> List[Int] { [] }\n");
}

#[test]
fn empty_arg_list_still_works() {
    assert_parses("fn f() -> Int { 1 }\nfn main() -> Int { f() }\n");
}

#[test]
fn double_trailing_comma_still_errors() {
    // Sanity: we allow ONE trailing comma, not unbounded.
    assert!(parse_source("fn f(x :: Int,,) -> Int { x }\n").is_err());
}
