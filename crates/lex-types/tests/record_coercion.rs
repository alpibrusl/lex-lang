//! Anonymous record literals coerce to nominal record aliases at any
//! position, not only function returns (closes #79). Two distinct
//! nominal types with the same shape stay nominally distinct.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;

fn check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).map(|_| ())
}

#[test]
fn record_literal_coerces_at_function_argument_position() {
    let src = r#"
type Community = { data_status :: Str, score :: Float }

fn show(c :: Community) -> Str { c.data_status }

fn main() -> Str {
  show({ data_status: "ok", score: 0.9 })
}
"#;
    check(src).expect("should accept");
}

#[test]
fn record_literal_coerces_as_nested_field_of_outer_record() {
    let src = r#"
type Inner = { x :: Int, y :: Int }
type Outer = { inner :: Inner, label :: Str }

fn build() -> Outer {
  { inner: { x: 1, y: 2 }, label: "hi" }
}
"#;
    check(src).expect("should accept");
}

#[test]
fn record_literal_coerces_at_function_return() {
    // The pre-existing return-position case still works after the
    // refactor.
    let src = r#"
type Pair = { fst :: Int, snd :: Int }

fn make() -> Pair { { fst: 1, snd: 2 } }
"#;
    check(src).expect("should accept");
}

#[test]
fn record_literal_coerces_in_let_binding_with_type_annotation() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn main() -> Int {
  let p :: Pt := { x: 1, y: 2 }
  p.x
}
"#;
    check(src).expect("should accept");
}

#[test]
fn record_literal_coerces_as_list_element() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn pts() -> List[Pt] {
  [{ x: 1, y: 2 }, { x: 3, y: 4 }]
}
"#;
    check(src).expect("should accept");
}

#[test]
fn declared_field_order_is_irrelevant() {
    // `type Community` declares fields in {score, data_status}; the
    // literal lists them in {data_status, score}. They unify because
    // record unification is key-based, not positional.
    let src = r#"
type Community = { score :: Float, data_status :: Str }

fn show() -> Community { { data_status: "ok", score: 0.9 } }
"#;
    check(src).expect("should accept regardless of declared field order");
}

#[test]
fn nominal_vs_nominal_still_rejects() {
    // Two type aliases with identical record shapes are still
    // nominally distinct; the coercion only fires when one side is
    // a bare record literal.
    let src = r#"
type Apple = { weight :: Int }
type Box = { weight :: Int }

fn ship(b :: Box) -> Int { b.weight }
fn make_apple() -> Apple { { weight: 5 } }

fn main() -> Int { ship(make_apple()) }
"#;
    let errs = check(src).expect_err("should reject Apple where Box expected");
    let msg = format!("{errs:?}");
    assert!(msg.contains("Apple") || msg.contains("Box"), "msg: {msg}");
}

#[test]
fn missing_field_still_rejects() {
    let src = r#"
type Community = { data_status :: Str, score :: Float }

fn show(c :: Community) -> Str { c.data_status }

fn main() -> Str {
  show({ data_status: "ok" })
}
"#;
    check(src).expect_err("should reject incomplete record literal");
}

#[test]
fn extra_field_still_rejects() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn id(p :: Pt) -> Pt { p }

fn main() -> Pt { id({ x: 1, y: 2, z: 3 }) }
"#;
    check(src).expect_err("should reject record literal with extra field");
}

#[test]
fn wrong_field_type_still_rejects() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn id(p :: Pt) -> Pt { p }

fn main() -> Pt { id({ x: 1, y: "no" }) }
"#;
    check(src).expect_err("should reject wrong-typed field");
}
