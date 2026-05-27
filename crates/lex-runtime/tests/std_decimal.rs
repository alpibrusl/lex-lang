//! Integration tests for `std.decimal` (#574).
//!
//! Decimal values are `{ coefficient :: Int, exponent :: Int }` records
//! representing `coefficient × 10^exponent`.  All arithmetic is exact;
//! precision loss only happens at `round_to` with an explicit mode.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn decimal(coef: i64, exp: i64) -> Value {
    use indexmap::IndexMap;
    use smol_str::SmolStr;
    let mut fields: IndexMap<SmolStr, Value> = IndexMap::new();
    fields.insert("coefficient".into(), Value::Int(coef));
    fields.insert("exponent".into(), Value::Int(exp));
    Value::record_interned(fields)
}

fn i(n: i64) -> Value { Value::Int(n) }
fn b(v: bool) -> Value { Value::Bool(v) }
fn s(v: &str) -> Value { Value::Str(v.into()) }

const PRELUDE: &str = r#"import "std.decimal" as d
"#;

fn src(body: &str) -> String { format!("{PRELUDE}{body}") }

// ── constructors ────────────────────────────────────────────────────────────

#[test]
fn decimal_constructor() {
    let code = src("fn t() -> { coefficient :: Int, exponent :: Int } { d.decimal(12345, -2) }");
    assert_eq!(run(&code, "t", vec![]), decimal(12345, -2));
}

#[test]
fn zero_and_one() {
    let code = src(r#"
fn z() -> { coefficient :: Int, exponent :: Int } { d.zero() }
fn o() -> { coefficient :: Int, exponent :: Int } { d.one() }
"#);
    assert_eq!(run(&code, "z", vec![]), decimal(0, 0));
    assert_eq!(run(&code, "o", vec![]), decimal(1, 0));
}

#[test]
fn from_int() {
    let code = src("fn t(n :: Int) -> { coefficient :: Int, exponent :: Int } { d.from_int(n) }");
    assert_eq!(run(&code, "t", vec![i(42)]), decimal(42, 0));
    assert_eq!(run(&code, "t", vec![i(-7)]), decimal(-7, 0));
    assert_eq!(run(&code, "t", vec![i(0)]),  decimal(0, 0));
}

#[test]
fn pow10() {
    let code = src("fn t(n :: Int) -> Int { d.pow10(n) }");
    assert_eq!(run(&code, "t", vec![i(0)]),  i(1));
    assert_eq!(run(&code, "t", vec![i(1)]),  i(10));
    assert_eq!(run(&code, "t", vec![i(3)]),  i(1000));
    assert_eq!(run(&code, "t", vec![i(18)]), i(1_000_000_000_000_000_000i64));
}

// ── arithmetic ──────────────────────────────────────────────────────────────

#[test]
fn add_same_exponent() {
    // 1.25 + 0.75 = 2.00  (same scale -2)
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.add(a, b) }
"#);
    let a = decimal(125, -2);
    let b_val = decimal(75, -2);
    assert_eq!(run(&code, "t", vec![a, b_val]), decimal(200, -2));
}

#[test]
fn add_different_exponents() {
    // 1.5 (150 × 10^-2) + 0.005 (5 × 10^-3) = 1.505 (1505 × 10^-3)
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.add(a, b) }
"#);
    let a = decimal(150, -2);
    let b_val = decimal(5, -3);
    assert_eq!(run(&code, "t", vec![a, b_val]), decimal(1505, -3));
}

#[test]
fn sub_basic() {
    // 2.00 - 0.75 = 1.25
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.sub(a, b) }
"#);
    let a = decimal(200, -2);
    let b_val = decimal(75, -2);
    assert_eq!(run(&code, "t", vec![a, b_val]), decimal(125, -2));
}

#[test]
fn mul_basic() {
    // 1.25 × 0.0005 = 0.000625  (125 × 10^-2 × 5 × 10^-4 = 625 × 10^-6)
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.mul(a, b) }
"#);
    let a = decimal(125, -2);
    let b_val = decimal(5, -4);
    assert_eq!(run(&code, "t", vec![a, b_val]), decimal(625, -6));
}

#[test]
fn fee_calculation() {
    // USD 1250.00 × 0.05% commission = 0.625 → round HalfUp to -2 → 0.63
    // notional: 125000 × 10^-2 = 1250.00
    // rate:     5 × 10^-4 = 0.0005
    // fee:      625000 × 10^-6 = 0.625000
    // rounded:  63 × 10^-2 = 0.63
    let code = src(r#"
fn t() -> { coefficient :: Int, exponent :: Int } {
  let notional := d.decimal(125000, -2)
  let rate     := d.decimal(5, -4)
  let fee      := d.mul(notional, rate)
  d.round_to(fee, -2, "HalfUp")
}
"#);
    assert_eq!(run(&code, "t", vec![]), decimal(63, -2));
}

// ── comparison ──────────────────────────────────────────────────────────────

#[test]
fn compare_equal() {
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int }) -> Int { d.compare(a, b) }
"#);
    assert_eq!(run(&code, "t", vec![decimal(100, -2), decimal(1, 0)]), i(0));
}

#[test]
fn compare_lt_gt() {
    let code = src(r#"
fn t(a :: { coefficient :: Int, exponent :: Int },
     b :: { coefficient :: Int, exponent :: Int }) -> Int { d.compare(a, b) }
"#);
    assert_eq!(run(&code, "t", vec![decimal(99, -2), decimal(1, 0)]), i(-1));
    assert_eq!(run(&code, "t", vec![decimal(101, -2), decimal(1, 0)]), i(1));
}

// ── predicates ──────────────────────────────────────────────────────────────

#[test]
fn predicates() {
    let code = src(r#"
fn pos(d1 :: { coefficient :: Int, exponent :: Int }) -> Bool { d.is_positive(d1) }
fn neg(d1 :: { coefficient :: Int, exponent :: Int }) -> Bool { d.is_negative(d1) }
fn zer(d1 :: { coefficient :: Int, exponent :: Int }) -> Bool { d.is_zero(d1) }
"#);
    assert_eq!(run(&code, "pos", vec![decimal(5, -1)]),   b(true));
    assert_eq!(run(&code, "pos", vec![decimal(-5, -1)]),  b(false));
    assert_eq!(run(&code, "neg", vec![decimal(-5, -1)]),  b(true));
    assert_eq!(run(&code, "neg", vec![decimal(5, -1)]),   b(false));
    assert_eq!(run(&code, "zer", vec![decimal(0, 3)]),    b(true));
    assert_eq!(run(&code, "zer", vec![decimal(1, 0)]),    b(false));
}

// ── transformers ────────────────────────────────────────────────────────────

#[test]
fn negate_and_abs() {
    let code = src(r#"
fn neg(d1 :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.negate(d1) }
fn ab(d1 :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.abs(d1) }
"#);
    assert_eq!(run(&code, "neg", vec![decimal(125, -2)]), decimal(-125, -2));
    assert_eq!(run(&code, "neg", vec![decimal(-63, -2)]), decimal(63, -2));
    assert_eq!(run(&code, "ab",  vec![decimal(-63, -2)]), decimal(63, -2));
    assert_eq!(run(&code, "ab",  vec![decimal(63, -2)]),  decimal(63, -2));
}

#[test]
fn normalize_removes_trailing_zeros() {
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int })
  -> { coefficient :: Int, exponent :: Int } { d.normalize(d1) }
"#);
    // 200 × 10^-2 = 2.00 → normalize → 2 × 10^0
    assert_eq!(run(&code, "t", vec![decimal(200, -2)]), decimal(2, 0));
    // 1500 × 10^-3 = 1.500 → 15 × 10^-1
    assert_eq!(run(&code, "t", vec![decimal(1500, -3)]), decimal(15, -1));
    // 0 → 0 × 10^0
    assert_eq!(run(&code, "t", vec![decimal(0, -5)]), decimal(0, 0));
}

// ── rounding ────────────────────────────────────────────────────────────────

#[test]
fn round_half_up() {
    // 0.625 (625 × 10^-3) rounded HalfUp to -2 → 0.63 (63 × 10^-2)
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "HalfUp")
}
"#);
    assert_eq!(run(&code, "t", vec![decimal(625, -3)]), decimal(63, -2));
}

#[test]
fn round_half_down() {
    // 0.625 rounded HalfDown to -2 → 0.62 (rounds down at exactly half)
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "HalfDown")
}
"#);
    assert_eq!(run(&code, "t", vec![decimal(625, -3)]), decimal(62, -2));
}

#[test]
fn round_half_even() {
    // 0.625 → nearest even at -2: 62 is even → 0.62
    // 0.635 → nearest even at -2: 64 is even → 0.64
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "HalfEven")
}
"#);
    assert_eq!(run(&code, "t", vec![decimal(625, -3)]), decimal(62, -2));
    assert_eq!(run(&code, "t", vec![decimal(635, -3)]), decimal(64, -2));
}

#[test]
fn round_floor_ceiling() {
    let code = src(r#"
fn floor_fn(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "Floor")
}
fn ceil_fn(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "Ceiling")
}
"#);
    // Positive: 0.627 → Floor → 0.62, Ceiling → 0.63
    assert_eq!(run(&code, "floor_fn", vec![decimal(627, -3)]), decimal(62, -2));
    assert_eq!(run(&code, "ceil_fn",  vec![decimal(627, -3)]), decimal(63, -2));
    // Negative: -0.627 → Floor → -0.63, Ceiling → -0.62
    assert_eq!(run(&code, "floor_fn", vec![decimal(-627, -3)]), decimal(-63, -2));
    assert_eq!(run(&code, "ceil_fn",  vec![decimal(-627, -3)]), decimal(-62, -2));
}

#[test]
fn round_exact_no_rounding() {
    // Rounding 0.6 to -2 (gaining precision) — exact, no rounding applied
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> { coefficient :: Int, exponent :: Int } {
  d.round_to(d1, -2, "HalfUp")
}
"#);
    // 6 × 10^-1 = 0.6 → round to -2 → 60 × 10^-2 = 0.60 (exact)
    assert_eq!(run(&code, "t", vec![decimal(6, -1)]), decimal(60, -2));
}

// ── to_str ──────────────────────────────────────────────────────────────────

#[test]
fn to_str_fractional() {
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> Str { d.to_str(d1) }
"#);
    assert_eq!(run(&code, "t", vec![decimal(12345, -2)]), s("123.45"));
    assert_eq!(run(&code, "t", vec![decimal(63, -2)]),    s("0.63"));
    assert_eq!(run(&code, "t", vec![decimal(-63, -2)]),   s("-0.63"));
    assert_eq!(run(&code, "t", vec![decimal(0, -2)]),     s("0.00"));
}

#[test]
fn to_str_integer() {
    let code = src(r#"
fn t(d1 :: { coefficient :: Int, exponent :: Int }) -> Str { d.to_str(d1) }
"#);
    assert_eq!(run(&code, "t", vec![decimal(42, 0)]),   s("42"));
    assert_eq!(run(&code, "t", vec![decimal(7, 2)]),    s("700"));
    assert_eq!(run(&code, "t", vec![decimal(-5, 0)]),   s("-5"));
}
