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

// ===== #439 — parametric record aliases =====
//
// `type Foo[T] = { ... }` parametric aliases were previously rejected
// from accepting an anonymous record literal at the use site: the
// unfold rule walked the `Ty::Con(_, args)` head only when `args` was
// empty, so `Box[Str]` never reached the structural record-coercion
// case. The fix substitutes the actuals (`args`) for the formal
// `Ty::Var(i)` slots in the alias body before unfolding.

#[test]
fn parametric_record_alias_coerces_at_function_return() {
    // The minimal reproducer from #439.
    let src = r#"
type Box[T] = { value :: T }

fn wrap_str(s :: Str) -> Box[Str] {
  { value: s }
}
"#;
    check(src).expect("should accept Box[Str] from record literal");
}

#[test]
fn parametric_record_alias_coerces_at_argument_position() {
    let src = r#"
type Box[T] = { value :: T }

fn unbox_int(b :: Box[Int]) -> Int { b.value }

fn main() -> Int {
  unbox_int({ value: 42 })
}
"#;
    check(src).expect("should accept Box[Int] at argument position");
}

#[test]
fn parametric_record_alias_field_access_substitutes_args() {
    // `b.value` on `b :: Box[Str]` should resolve to `Str`, not the
    // formal `T`. Pre-fix, this either failed to type-check or bound
    // the inferred return to the formal param's index.
    let src = r#"
type Box[T] = { value :: T }

fn unbox(b :: Box[Str]) -> Str { b.value }
"#;
    check(src).expect("Box[Str].value should type as Str");
}

#[test]
fn parametric_record_alias_with_multiple_fields() {
    // The pagination shape from the issue body.
    let src = r#"
type Page[T] = {
  items  :: List[T],
  offset :: Int,
  limit  :: Int,
  total  :: Int,
}

fn empty_page() -> Page[Int] {
  { items: [], offset: 0, limit: 0, total: 0 }
}
"#;
    check(src).expect("multi-field parametric record alias should coerce");
}

#[test]
fn parametric_record_alias_nested_generic_field() {
    // `Page[T]` with `items :: List[T]` — exercises substitution
    // walking into a nested `List[Ty::Var(0)]`.
    let src = r#"
type Page[T] = { items :: List[T], total :: Int }

fn one_int_page() -> Page[Int] {
  { items: [1, 2, 3], total: 3 }
}
"#;
    check(src).expect("nested List[T] should substitute to List[Int]");
}

#[test]
fn parametric_record_alias_wrong_inner_type_still_rejects() {
    // Substituting `T → Int` means `value :: Int`; a Str literal
    // must still be rejected.
    let src = r#"
type Box[T] = { value :: T }

fn bad() -> Box[Int] { { value: "not an int" } }
"#;
    check(src).expect_err("should reject Str where Box[Int] expected Int");
}

#[test]
fn distinct_parametric_aliases_with_same_shape_still_reject() {
    // Nominal distinction extends to parametric aliases: `Box[Int]`
    // and `Crate[Int]` have the same unfolded shape but are not
    // interchangeable.
    let src = r#"
type Box[T]   = { value :: T }
type Crate[T] = { value :: T }

fn ship(c :: Crate[Int]) -> Int { c.value }
fn make_box() -> Box[Int] { { value: 7 } }

fn main() -> Int { ship(make_box()) }
"#;
    let errs = check(src).expect_err("should reject Box where Crate expected");
    let msg = format!("{errs:?}");
    assert!(msg.contains("Box") || msg.contains("Crate"), "msg: {msg}");
}

#[test]
fn parametric_record_alias_in_let_binding() {
    let src = r#"
type Box[T] = { value :: T }

fn main() -> Int {
  let b :: Box[Int] := { value: 9 }
  b.value
}
"#;
    check(src).expect("let-binding annotation should coerce parametric alias");
}

#[test]
fn parametric_record_alias_as_list_element() {
    let src = r#"
type Box[T] = { value :: T }

fn boxes() -> List[Box[Int]] {
  [{ value: 1 }, { value: 2 }]
}
"#;
    check(src).expect("list element should coerce to Box[Int]");
}
