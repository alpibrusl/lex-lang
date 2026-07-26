//! Integration tests for `std.df` (#427). Each test builds an
//! `arrow.Table` via `std.arrow`, runs a Polars-backed kernel, and
//! checks the resulting Table shape / values.
//!
//! Gated on the `df` feature (Polars). With `--no-default-features`
//! the `std.df` ops aren't compiled in, so these tests are skipped.
#![cfg(feature = "df")]

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1
            => args.into_iter().next().unwrap(),
        other => panic!("expected Ok(_), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.list"  as list
import "std.arrow" as arrow
import "std.df"    as df

# 6 rows; g cycles ["a","b","a","b","a","b"]; x = [1..6], y = [10..60].
fn build() -> Result[Table, Str] {
  let xs := list.cons(1, list.cons(2, list.cons(3, list.cons(4, list.cons(5, list.cons(6, []))))))
  let ys := list.cons(10, list.cons(20, list.cons(30, list.cons(40, list.cons(50, list.cons(60, []))))))
  let cols := list.cons(("x", xs), list.cons(("y", ys), []))
  arrow.from_int_columns(cols)
}

fn build_g() -> Result[Table, Str] {
  let xs := list.cons(1, list.cons(2, list.cons(3, list.cons(4, list.cons(5, list.cons(6, []))))))
  let ys := list.cons(10, list.cons(20, list.cons(30, list.cons(40, list.cons(50, list.cons(60, []))))))
  let gs := list.cons("a", list.cons("b", list.cons("a", list.cons("b", list.cons("a", list.cons("b", []))))))
  match arrow.from_int_columns(list.cons(("x", xs), list.cons(("y", ys), []))) {
    Err(e) => Err(e),
    Ok(t) => match arrow.from_str_columns(list.cons(("g", gs), [])) {
      Err(e) => Err(e),
      Ok(_) => Err("placeholder"),
    },
  }
}

fn filter_eq_3_nrows() -> Int {
  match build() {
    Err(_) => -1,
    Ok(t) => match df.filter_eq_int(t, "x", 3) {
      Err(_) => -2,
      Ok(t2) => arrow.nrows(t2),
    },
  }
}

fn filter_gt_3_nrows() -> Int {
  match build() {
    Err(_) => -1,
    Ok(t) => match df.filter_gt_int(t, "x", 3) {
      Err(_) => -2,
      Ok(t2) => arrow.nrows(t2),
    },
  }
}

fn sort_first_x_desc() -> Int {
  match build() {
    Err(_) => -1,
    Ok(t) => match df.sort_by(t, "x", false) {
      Err(_) => -2,
      Ok(t2) => match arrow.col_sum_int(arrow.head(t2, 1), "x") {
        Ok(s) => s,
        Err(_) => -3,
      },
    },
  }
}

# group_by single-key (the g column) with sum(x) + mean(y).
# Need a Table with three columns — use from_str_columns isn't ideal because
# we'd need a mixed builder. Instead, construct the g column separately by
# read_csv in a real test; for the unit test we sort-by-x then verify
# rather than build mixed-type. Keep this simple.
fn group_by_x_self() -> Int {
  # group by "x" (each row distinct), sum(y). 6 distinct x values => 6 rows.
  match build() {
    Err(_) => -1,
    Ok(t) => match df.group_by_agg(
      t,
      list.cons("x", []),
      list.cons(("sum_y", "y", "sum"), [])
    ) {
      Err(_) => -2,
      Ok(t2) => arrow.nrows(t2),
    },
  }
}
"#;

#[test]
fn df_filter_eq_int() {
    assert_eq!(run(SRC, "filter_eq_3_nrows", vec![]), Value::Int(1));
}

#[test]
fn df_filter_gt_int() {
    // x > 3 → rows 4, 5, 6 → 3 rows
    assert_eq!(run(SRC, "filter_gt_3_nrows", vec![]), Value::Int(3));
}

#[test]
fn df_sort_by_desc() {
    // Sorted desc, first row x = 6
    assert_eq!(run(SRC, "sort_first_x_desc", vec![]), Value::Int(6));
}

#[test]
fn df_group_by_agg() {
    // 6 distinct x values → 6 output rows
    assert_eq!(run(SRC, "group_by_x_self", vec![]), Value::Int(6));
}

/// Direct dispatch round-trip — useful for catching arrow ↔ polars
/// conversion bugs without going through the bytecode VM.
#[test]
fn df_kernels_via_direct_dispatch() {
    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};

    let schema = Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Int64, false),
    ]);
    let xs = Int64Array::from(vec![1, 2, 3, 4, 5, 6]);
    let ys = Int64Array::from(vec![10, 20, 30, 40, 50, 60]);
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(xs), Arc::new(ys)],
    ).unwrap();
    let table = Value::ArrowTable(Arc::new(batch));

    // filter_gt_int x > 4 → 2 rows (5, 6)
    let r = lex_runtime::df::dispatch(
        "filter_gt_int",
        &[table.clone(), Value::Str("x".into()), Value::Int(4)],
    ).unwrap().unwrap();
    let out = unwrap_ok(r);
    if let Value::ArrowTable(t) = out {
        assert_eq!(t.num_rows(), 2);
    } else {
        panic!("expected ArrowTable, got {out:?}");
    }
}

// ===== #433 — string / float / null filter predicates =====

/// Build a 6-row mixed-type Table programmatically for the new
/// predicates. Columns: x: Int64 [1..6], y: Float64 [1.0..6.0], g:
/// Utf8 ["a","b","a","b","a","b"]. Includes one null in `z` (Int64)
/// at rows 1 and 3.
fn make_mixed_batch() -> Value {
    use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("g", DataType::Utf8, false),
        Field::new("z", DataType::Int64, true),
    ]));
    let xs = Int64Array::from(vec![1_i64, 2, 3, 4, 5, 6]);
    let ys = Float64Array::from(vec![1.0_f64, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let gs = StringArray::from(vec!["a", "b", "a", "b", "a", "b"]);
    let zs = Int64Array::from(vec![None, Some(20), None, Some(40), Some(50), Some(60)]);
    let batch = RecordBatch::try_new(
        schema,
        vec![Arc::new(xs), Arc::new(ys), Arc::new(gs), Arc::new(zs)],
    ).unwrap();
    Value::ArrowTable(Arc::new(batch))
}

fn nrows_of(v: Value) -> usize {
    match v {
        Value::ArrowTable(t) => t.num_rows(),
        other => panic!("expected ArrowTable, got {other:?}"),
    }
}

fn unwrap_err(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            match args.into_iter().next().unwrap() {
                Value::Str(s) => s.to_string(),
                other => panic!("Err payload not Str: {other:?}"),
            }
        }
        other => panic!("expected Err(_), got {other:?}"),
    }
}

#[test]
fn df_filter_eq_str() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_eq_str",
        &[t, Value::Str("g".into()), Value::Str("a".into())],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 3);
}

#[test]
fn df_filter_in_str() {
    let t = make_mixed_batch();
    let needles = Value::List([Value::Str("a".into()), Value::Str("z".into())].into_iter().collect());
    let r = lex_runtime::df::dispatch(
        "filter_in_str",
        &[t, Value::Str("g".into()), needles],
    ).unwrap().unwrap();
    // "z" doesn't exist; "a" matches 3 rows.
    assert_eq!(nrows_of(unwrap_ok(r)), 3);
}

#[test]
fn df_filter_in_str_empty_list_is_empty_result() {
    let t = make_mixed_batch();
    let needles = Value::List(std::collections::VecDeque::new());
    let r = lex_runtime::df::dispatch(
        "filter_in_str",
        &[t, Value::Str("g".into()), needles],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 0);
}

#[test]
fn df_filter_eq_str_on_int_column_is_err() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_eq_str",
        &[t, Value::Str("x".into()), Value::Str("1".into())],
    ).unwrap().unwrap();
    let msg = unwrap_err(r);
    assert!(msg.contains("expected") && msg.contains("Utf8"),
        "expected type-mismatch with Utf8, got: {msg}");
}

#[test]
fn df_filter_lt_float() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_lt_float",
        &[t, Value::Str("y".into()), Value::Float(3.5)],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 3);
}

#[test]
fn df_filter_gt_float() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_gt_float",
        &[t, Value::Str("y".into()), Value::Float(3.5)],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 3);
}

#[test]
fn df_filter_eq_float() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_eq_float",
        &[t, Value::Str("y".into()), Value::Float(4.0)],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 1);
}

#[test]
fn df_filter_isnull_and_notnull() {
    let t = make_mixed_batch();

    // z has nulls at rows 1 and 3 → filter_isnull → 2 rows.
    let r = lex_runtime::df::dispatch(
        "filter_isnull", &[t.clone(), Value::Str("z".into())],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 2, "z has 2 nulls");

    // filter_notnull → 4 rows.
    let r = lex_runtime::df::dispatch(
        "filter_notnull", &[t, Value::Str("z".into())],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 4, "z has 4 non-null values");
}

#[test]
fn df_filter_isnull_unknown_column_is_err() {
    let t = make_mixed_batch();
    let r = lex_runtime::df::dispatch(
        "filter_isnull", &[t, Value::Str("nope".into())],
    ).unwrap().unwrap();
    let msg = unwrap_err(r);
    assert!(msg.contains("nope"), "error should name missing column: {msg}");
}

#[test]
fn df_drop_nulls() {
    let t = make_mixed_batch();
    let cols = Value::List([Value::Str("z".into())].into_iter().collect());
    let r = lex_runtime::df::dispatch(
        "drop_nulls", &[t, cols],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 4, "drop_nulls on z removes 2 rows");
}

#[test]
fn df_drop_nulls_empty_col_list_is_noop() {
    let t = make_mixed_batch();
    let empty_cols = Value::List(std::collections::VecDeque::new());
    let r = lex_runtime::df::dispatch(
        "drop_nulls", &[t, empty_cols],
    ).unwrap().unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 6, "empty col list is a no-op");
}

/// The Polars-backed CSV reader behind `arrow.read_csv` (df feature):
/// dtype normalisation to the v1 surface and null preservation.
///
/// The file exercises the three inference paths that differ from the
/// old arrow-rs reader: a Boolean-inferred column (cast to Utf8), an
/// Int64 column with a gap (null must survive so `df.filter_isnull`
/// can see it), and plain Utf8/Float64 columns (untouched).
#[test]
fn df_read_csv_polars_normalises_dtypes_and_keeps_nulls() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.csv");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "n,flag,name,score").unwrap();
    writeln!(f, "1,true,alice,1.5").unwrap();
    writeln!(f, ",false,bob,2.5").unwrap();
    writeln!(f, "3,true,carol,3.5").unwrap();
    drop(f);

    let t = lex_runtime::df::read_csv_at_polars(&path).expect("read");
    let rb = match &t {
        Value::ArrowTable(rb) => rb.clone(),
        other => panic!("expected ArrowTable, got {other:?}"),
    };
    assert_eq!(rb.num_rows(), 3);
    let dtype_of = |name: &str| {
        rb.schema()
            .field_with_name(name)
            .unwrap_or_else(|_| panic!("column {name} missing"))
            .data_type()
            .clone()
    };
    assert_eq!(dtype_of("n"), arrow_schema::DataType::Int64);
    assert_eq!(dtype_of("flag"), arrow_schema::DataType::Utf8, "Boolean casts to Utf8");
    assert_eq!(dtype_of("name"), arrow_schema::DataType::Utf8);
    assert_eq!(dtype_of("score"), arrow_schema::DataType::Float64);

    // The empty cell in `n` must survive as a null the df kernels can see.
    let r = lex_runtime::df::dispatch("filter_isnull", &[t.clone(), Value::Str("n".into())])
        .unwrap()
        .unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 1, "one null row in n");

    // And the reduction path still works over the normalised table.
    let r = lex_runtime::df::dispatch("filter_eq_str", &[t, Value::Str("flag".into()), Value::Str("true".into())])
        .unwrap()
        .unwrap();
    assert_eq!(nrows_of(unwrap_ok(r)), 2, "flag=true rows via the cast Utf8 column");
}
