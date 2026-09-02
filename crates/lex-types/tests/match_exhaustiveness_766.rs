//! #766: `lex check` must reject a `match` that does not cover every
//! value of its scrutinee's type. Before this the checker only
//! rejected an arm-less `match`; every other hole passed and panicked
//! at runtime with `non-exhaustive match`.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{check_program, TypeError};

fn check(src: &str) -> Result<(), Vec<TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

/// The `missing` witnesses of the first non-exhaustive-match error.
fn missing(src: &str) -> Vec<String> {
    let errs = check(src).expect_err("expected a type error");
    for e in &errs {
        if let TypeError::NonExhaustiveMatch { missing, .. } = e {
            return missing.clone();
        }
    }
    panic!("no NonExhaustiveMatch among: {errs:#?}");
}

fn assert_ok(src: &str) {
    check(src).unwrap_or_else(|errs| panic!("expected exhaustive; got: {errs:#?}"));
}

// --- the cases from the issue ---------------------------------------

#[test]
fn option_missing_none() {
    let src = r#"
fn unwrap_or_zero(x :: Option[Int]) -> Int {
  match x {
    Some(v) => v,
  }
}
"#;
    assert_eq!(missing(src), vec!["None"]);
}

#[test]
fn local_union_missing_two_of_three() {
    let src = r#"
type Shape = Circle(Int) | Square(Int) | Triangle(Int)

fn area(s :: Shape) -> Int {
  match s {
    Circle(r) => r * 3,
  }
}
"#;
    assert_eq!(missing(src), vec!["Square(_)", "Triangle(_)"]);
}

#[test]
fn tag_only_union_missing_one() {
    let src = r#"
type Cmd = Hit | Get | Reset

fn run(c :: Cmd) -> Int {
  match c {
    Hit => 1,
    Get => 2,
  }
}
"#;
    assert_eq!(missing(src), vec!["Reset"]);
}

#[test]
fn fully_covered_union_is_ok() {
    assert_ok(r#"
type Status = Healthy | Sick | Recovering

fn label(s :: Status) -> Str {
  match s {
    Healthy => "ok",
    Sick => "nope",
    Recovering => "wait",
  }
}
"#);
}

#[test]
fn wildcard_arm_covers_everything() {
    assert_ok(r#"
type Shape = Circle(Int) | Square(Int) | Triangle(Int)

fn area(s :: Shape) -> Int {
  match s {
    Circle(r) => r * 3,
    _ => 0,
  }
}
"#);
}

#[test]
fn binder_arm_covers_everything() {
    assert_ok(r#"
fn f(x :: Option[Int]) -> Int {
  match x {
    Some(v) => v,
    other => 0,
  }
}
"#);
}

// --- nested payloads --------------------------------------------------

#[test]
fn nested_payload_hole_is_reported() {
    let src = r#"
fn f(x :: Option[Bool]) -> Int {
  match x {
    Some(true) => 1,
    None => 0,
  }
}
"#;
    assert_eq!(missing(src), vec!["Some(false)"]);
}

#[test]
fn nested_payload_covered_by_binder_is_ok() {
    assert_ok(r#"
fn f(x :: Option[Int]) -> Int {
  match x {
    Some(1) => 10,
    Some(n) => n,
    None => 0,
  }
}
"#);
}

#[test]
fn nested_union_inside_union() {
    let src = r#"
type Shape = Circle(Int) | Square(Int)

fn f(x :: Option[Shape]) -> Int {
  match x {
    Some(Circle(r)) => r,
    None => 0,
  }
}
"#;
    assert_eq!(missing(src), vec!["Some(Square(_))"]);
}

#[test]
fn result_missing_err() {
    let src = r#"
fn f(r :: Result[Int, Str]) -> Int {
  match r {
    Ok(v) => v,
  }
}
"#;
    assert_eq!(missing(src), vec!["Err(_)"]);
}

// --- Bool, Unit, Int -------------------------------------------------

#[test]
fn bool_needs_both_literals() {
    let src = r#"
fn f(b :: Bool) -> Int {
  match b {
    true => 1,
  }
}
"#;
    assert_eq!(missing(src), vec!["false"]);
    assert_ok(r#"
fn g(b :: Bool) -> Int {
  match b {
    true => 1,
    false => 0,
  }
}
"#);
}

#[test]
fn int_literals_need_a_wildcard() {
    let src = r#"
fn f(n :: Int) -> Str {
  match n {
    0 => "zero",
    1 => "one",
  }
}
"#;
    assert_eq!(missing(src), vec!["_"]);
    assert_ok(r#"
fn g(n :: Int) -> Str {
  match n {
    0 => "zero",
    _ => "many",
  }
}
"#);
}

#[test]
fn str_literals_need_a_wildcard() {
    let src = r#"
fn f(s :: Str) -> Int {
  match s {
    "a" => 1,
  }
}
"#;
    assert_eq!(missing(src), vec!["_"]);
}

// --- tuples and records ----------------------------------------------

#[test]
fn tuple_of_options_reports_the_uncovered_combination() {
    let src = r#"
fn f(p :: (Option[Int], Bool)) -> Int {
  match p {
    (Some(v), _) => v,
    (None, true) => 1,
  }
}
"#;
    assert_eq!(missing(src), vec!["(None, false)"]);
    assert_ok(r#"
fn g(p :: (Option[Int], Bool)) -> Int {
  match p {
    (Some(v), _) => v,
    (None, true) => 1,
    (None, false) => 0,
  }
}
"#);
}

#[test]
fn multi_arg_constructor_payload_is_a_tuple() {
    let src = r#"
type Pair = Pair(Bool, Bool) | Nothing

fn f(p :: Pair) -> Int {
  match p {
    Pair(true, _) => 1,
    Nothing => 0,
  }
}
"#;
    assert_eq!(missing(src), vec!["Pair(false, _)"]);
}

#[test]
fn record_pattern_hole_names_the_field() {
    let src = r#"
fn f(r :: { flag :: Bool, n :: Int }) -> Int {
  match r {
    { flag: true } => 1,
  }
}
"#;
    assert_eq!(missing(src), vec!["{ flag: false }"]);
    assert_ok(r#"
fn g(r :: { flag :: Bool, n :: Int }) -> Int {
  match r {
    { flag: true } => 1,
    { flag: false, n: k } => k,
  }
}
"#);
}

#[test]
fn record_alias_scrutinee_is_unfolded() {
    let src = r#"
type Cfg = { flag :: Bool }

fn f(c :: Cfg) -> Int {
  match c {
    { flag: true } => 1,
  }
}
"#;
    assert_eq!(missing(src), vec!["{ flag: false }"]);
}

// --- generic and imported types --------------------------------------

#[test]
fn generic_union_instantiated_payload() {
    let src = r#"
type Box[T] = Full(T) | Empty

fn f(b :: Box[Bool]) -> Int {
  match b {
    Full(true) => 1,
    Empty => 0,
  }
}
"#;
    assert_eq!(missing(src), vec!["Full(false)"]);
}

#[test]
fn error_carries_the_rule_tag() {
    let src = r#"
fn f(x :: Option[Int]) -> Int {
  match x {
    Some(v) => v,
  }
}
"#;
    let errs = check(src).unwrap_err();
    assert_eq!(errs[0].rule_tag(), "non-exhaustive-match");
}

// --- recursive types (reported on the PR: stack overflow) ------------
//
// A wildcard arm accepts every constructor but names none, so it must
// never make the pass descend into a payload type. On a type that
// contains itself, doing so recursed until the stack ran out and
// aborted `lex check` on files with no hole at all.

#[test]
fn wildcard_arm_on_self_recursive_union_terminates() {
    let src = r#"
type Kind = Leaf(Int) | Pair((Kind, Kind)) | Many(List[Kind])

fn depth(k :: Kind) -> Int {
  match k {
    Leaf(_) => 0,
    _ => 1,
  }
}
"#;
    assert_ok(src);
}

#[test]
fn all_arms_named_on_self_recursive_union_terminates() {
    let src = r#"
type Kind = Leaf(Int) | Pair((Kind, Kind))

fn depth(k :: Kind) -> Int {
  match k {
    Leaf(_) => 0,
    Pair(p) => 1,
  }
}
"#;
    assert_ok(src);
}

#[test]
fn mutually_recursive_union_and_records_terminate() {
    // The lex-schema shape: a union whose payloads reach back to it
    // through a tuple and through a record whose field is a list.
    let src = r#"
type FieldKind = KStr | KBool | KArray((FieldKind, List[Int])) | KObject(ModelSchema)
type Field = { name :: Str, kind :: FieldKind }
type ModelSchema = { title :: Str, fields :: List[Field] }

fn name(k :: FieldKind) -> Str {
  match k {
    KStr => "str",
    KBool => "bool",
    KArray(_, _) => "array",
    KObject(sub) => sub.title,
  }
}

fn is_array(k :: FieldKind) -> Bool {
  match k {
    KArray(_, _) => true,
    _ => false,
  }
}

fn first_field_kind(m :: ModelSchema) -> Str {
  match m {
    { title: t } => t,
  }
}
"#;
    assert_ok(src);
}

#[test]
fn hole_inside_recursive_payload_is_still_found() {
    let src = r#"
type Kind = Leaf(Bool) | Pair((Kind, Kind))

fn f(k :: Kind) -> Int {
  match k {
    Leaf(true) => 0,
    Leaf(false) => 1,
    Pair((Leaf(_), _)) => 2,
  }
}
"#;
    assert_eq!(missing(src), vec!["Pair(Pair(_), _)"]);
}

#[test]
fn wildcard_row_inside_recursive_payload_covers_it() {
    let src = r#"
type Kind = Leaf(Bool) | Pair((Kind, Kind))

fn f(k :: Kind) -> Int {
  match k {
    Leaf(true) => 0,
    Leaf(false) => 1,
    Pair((Leaf(_), rest)) => 2,
    Pair(_) => 3,
  }
}
"#;
    assert_ok(src);
}
