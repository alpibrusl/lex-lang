//! Integration tests for `std.arrow` (#426 slice 1).
//!
//! Covers:
//! 1. Construction round-trip: `from_int_columns` builds a Table whose
//!    `nrows`, `ncols`, `col_names`, `col_type` match what we put in.
//! 2. Numeric reductions: `col_sum_int` / `col_mean` / `col_min_int` /
//!    `col_max_int` / `col_count` return the expected scalar.
//! 3. Slicing: `head`, `tail`, `slice` return a Table of the right shape.
//! 4. Projection: `select_cols`, `drop_col` preserve the requested
//!    columns and reject unknown names with `Err(_)`.
//! 5. Length-mismatch: `from_int_columns` with mismatched column lengths
//!    surfaces as `Err(_)`, not a runtime panic.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::VecDeque;
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

fn unwrap_some(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Some" && args.len() == 1
            => args.into_iter().next().unwrap(),
        other => panic!("expected Some(_), got {other:?}"),
    }
}

/// Two-int-column source: x = [10,20,30,40], y = [1,2,3,4].
const SRC_INT: &str = r#"
import "std.list"  as list
import "std.arrow" as arrow

fn build() -> Result[Table, Str] {
  let xs := list.cons(10, list.cons(20, list.cons(30, list.cons(40, []))))
  let ys := list.cons(1, list.cons(2, list.cons(3, list.cons(4, []))))
  arrow.from_int_columns(list.cons(("x", xs), list.cons(("y", ys), [])))
}

fn nrows() -> Int {
  match build() {
    Ok(t) => arrow.nrows(t),
    Err(_) => -1,
  }
}

fn ncols() -> Int {
  match build() {
    Ok(t) => arrow.ncols(t),
    Err(_) => -1,
  }
}

fn sum_x() -> Int {
  match build() {
    Ok(t) => match arrow.col_sum_int(t, "x") {
      Ok(s) => s,
      Err(_) => -1,
    },
    Err(_) => -1,
  }
}

fn mean_x() -> Float {
  match build() {
    Ok(t) => match arrow.col_mean(t, "x") {
      Ok(Some(m)) => m,
      _ => 0.0,
    },
    Err(_) => 0.0,
  }
}

fn min_x() -> Int {
  match build() {
    Ok(t) => match arrow.col_min_int(t, "x") {
      Ok(Some(m)) => m,
      _ => -1,
    },
    Err(_) => -1,
  }
}

fn max_y() -> Int {
  match build() {
    Ok(t) => match arrow.col_max_int(t, "y") {
      Ok(Some(m)) => m,
      _ => -1,
    },
    Err(_) => -1,
  }
}

fn head_nrows() -> Int {
  match build() {
    Ok(t) => arrow.nrows(arrow.head(t, 2)),
    Err(_) => -1,
  }
}

fn select_one_ncols() -> Int {
  match build() {
    Ok(t) => match arrow.select_cols(t, list.cons("y", [])) {
      Ok(t2) => arrow.ncols(t2),
      Err(_) => -1,
    },
    Err(_) => -1,
  }
}

fn drop_unknown_is_err() -> Bool {
  match build() {
    Ok(t) => match arrow.drop_col(t, "no_such") {
      Ok(_) => false,
      Err(_) => true,
    },
    Err(_) => false,
  }
}

# Length-mismatch: x has 4 rows, z has 3.
fn build_mismatch() -> Result[Table, Str] {
  let xs := list.cons(1, list.cons(2, list.cons(3, list.cons(4, []))))
  let zs := list.cons(1, list.cons(2, list.cons(3, [])))
  arrow.from_int_columns(list.cons(("x", xs), list.cons(("z", zs), [])))
}

fn build_mismatch_is_err() -> Bool {
  match build_mismatch() {
    Ok(_)  => false,
    Err(_) => true,
  }
}
"#;

#[test]
fn arrow_construction_and_introspection() {
    assert_eq!(run(SRC_INT, "nrows", vec![]), Value::Int(4));
    assert_eq!(run(SRC_INT, "ncols", vec![]), Value::Int(2));
}

#[test]
fn arrow_int_reductions() {
    assert_eq!(run(SRC_INT, "sum_x", vec![]), Value::Int(100));
    assert_eq!(run(SRC_INT, "mean_x", vec![]), Value::Float(25.0));
    assert_eq!(run(SRC_INT, "min_x", vec![]), Value::Int(10));
    assert_eq!(run(SRC_INT, "max_y", vec![]), Value::Int(4));
}

#[test]
fn arrow_slicing_and_projection() {
    assert_eq!(run(SRC_INT, "head_nrows", vec![]), Value::Int(2));
    assert_eq!(run(SRC_INT, "select_one_ncols", vec![]), Value::Int(1));
    assert_eq!(run(SRC_INT, "drop_unknown_is_err", vec![]), Value::Bool(true));
}

#[test]
fn arrow_length_mismatch_is_err_not_panic() {
    assert_eq!(run(SRC_INT, "build_mismatch_is_err", vec![]), Value::Bool(true));
}

/// `arrow.read_csv` end-to-end: write a small CSV to a temp dir, grant
/// `[fs_read]` scoped to that dir, read it back, run a reduction.
#[test]
fn arrow_read_csv_end_to_end() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.csv");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "x,y").unwrap();
    for i in 1..=10 {
        writeln!(f, "{i},{}", i * 10).unwrap();
    }
    drop(f);

    let src = r#"
import "std.arrow" as arrow
fn sum_x(p :: Str) -> [fs_read] Int {
  match arrow.read_csv(p) {
    Err(_) => -1,
    Ok(t) => match arrow.col_sum_int(t, "x") {
      Ok(s) => s,
      Err(_) => -2,
    },
  }
}
"#;
    // Build a policy that grants fs_read scoped to the temp dir.
    let mut policy = Policy::pure();
    policy.allow_effects.insert("fs_read".into());
    policy.allow_fs_read.push(dir.path().to_path_buf());

    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let res = vm.call("sum_x", vec![
        Value::Str(path.to_string_lossy().to_string().into()),
    ]).unwrap();
    assert_eq!(res, Value::Int(55));  // 1 + 2 + ... + 10
}

/// `arrow.read_csv` without an `--allow-fs-read` allowlist that covers
/// the path must surface as Err, not panic and not read the file.
#[test]
fn arrow_read_csv_outside_allow_list_is_err() {
    let src = r#"
import "std.arrow" as arrow
fn sum_x(p :: Str) -> [fs_read] Int {
  match arrow.read_csv(p) {
    Err(_) => -1,
    Ok(t) => match arrow.col_sum_int(t, "x") {
      Ok(s) => s,
      Err(_) => -2,
    },
  }
}
"#;
    // Grant fs_read effect but scope to /nonexistent, then attempt to
    // read /etc/passwd — must refuse before opening the file.
    let mut policy = Policy::pure();
    policy.allow_effects.insert("fs_read".into());
    policy.allow_fs_read.push("/nonexistent/path".into());

    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let _ = lex_types::check_program(&stages);
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let err = vm.call("sum_x", vec![
        Value::Str("/etc/passwd".into()),
    ]);
    // Effect-handler returns a host-level error; the VM surfaces that
    // as VmError. We want the message to mention the scope refusal.
    assert!(err.is_err(), "expected effect error, got {err:?}");
    let msg = format!("{:?}", err.err().unwrap());
    assert!(msg.contains("--allow-fs-read") || msg.contains("/etc/passwd"),
        "expected scope refusal, got: {msg}");
}

#[test]
fn arrow_kernels_match_native_arrow_directly() {
    // Build the table programmatically and run the kernel via dispatch
    // so we check the runtime entry-point in isolation from the Lex VM.
    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    let schema = Schema::new(vec![Field::new("x", DataType::Int64, false)]);
    let arr = Int64Array::from(vec![10_i64, 20, 30, 40]);
    let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(arr)]).unwrap();
    let table = Value::ArrowTable(Arc::new(batch));

    // Sum.
    let r = lex_runtime::arrow::dispatch(
        "col_sum_int", &[table.clone(), Value::Str("x".into())]).unwrap().unwrap();
    assert_eq!(unwrap_ok(r), Value::Int(100));

    // Mean.
    let r = lex_runtime::arrow::dispatch(
        "col_mean", &[table.clone(), Value::Str("x".into())]).unwrap().unwrap();
    assert_eq!(unwrap_some(unwrap_ok(r)), Value::Float(25.0));

    // Unknown column → Err, not panic.
    let r = lex_runtime::arrow::dispatch(
        "col_sum_int", &[table.clone(), Value::Str("nope".into())]).unwrap().unwrap();
    let msg = unwrap_err(r);
    assert!(msg.contains("nope"), "expected error to mention column name, got {msg}");

    // col_names.
    let r = lex_runtime::arrow::dispatch("col_names", std::slice::from_ref(&table)).unwrap().unwrap();
    let names: VecDeque<Value> = match r {
        Value::List(v) => v,
        other => panic!("expected List, got {other:?}"),
    };
    assert_eq!(names, VecDeque::from(vec![Value::Str("x".into())]));
}

// ===== #432 — Parquet + CSV-write I/O =====
//
// These tests go through the Rust API directly (`read_parquet_at`,
// `write_parquet_at`, `write_csv_at`) so we can exercise the kernel
// without standing up the VM + policy machinery. The end-to-end
// effect-handler path is covered by `parquet_round_trip_via_vm`.

/// Build a small RecordBatch for the I/O tests: x = [1,2,3], y = [10.0, 20.0, 30.0], g = ["a","b","c"].
fn small_batch() -> Arc<arrow_array::RecordBatch> {
    use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    let schema = Arc::new(Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("g", DataType::Utf8, false),
    ]));
    let rb = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1_i64, 2, 3])),
            Arc::new(Float64Array::from(vec![10.0_f64, 20.0, 30.0])),
            Arc::new(StringArray::from(vec!["a", "b", "c"])),
        ],
    )
    .unwrap();
    Arc::new(rb)
}

#[test]
fn parquet_write_read_round_trip() {
    let rb = small_batch();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");

    lex_runtime::arrow::write_parquet_at(&rb, &path).expect("write parquet");
    assert!(path.exists());

    let v = lex_runtime::arrow::read_parquet_at(&path).expect("read parquet");
    let rb2 = match v {
        Value::ArrowTable(t) => t,
        other => panic!("expected ArrowTable, got {other:?}"),
    };
    assert_eq!(rb2.num_rows(), 3);
    assert_eq!(rb2.num_columns(), 3);
    assert_eq!(
        rb2.schema().fields().iter().map(|f| f.name().as_str()).collect::<Vec<_>>(),
        vec!["x", "y", "g"],
    );

    // Values round-trip.
    let r = lex_runtime::arrow::dispatch(
        "col_sum_int", &[Value::ArrowTable(rb2.clone()), Value::Str("x".into())])
        .unwrap().unwrap();
    assert_eq!(unwrap_ok(r), Value::Int(6));
    let r = lex_runtime::arrow::dispatch(
        "col_sum_float", &[Value::ArrowTable(rb2), Value::Str("y".into())])
        .unwrap().unwrap();
    assert_eq!(unwrap_ok(r), Value::Float(60.0));
}

#[test]
fn parquet_read_cols_pushes_down_projection() {
    let rb = small_batch();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    lex_runtime::arrow::write_parquet_at(&rb, &path).unwrap();

    // Pull only "x" and "g" — "y" must not be in the result.
    let v = lex_runtime::arrow::read_parquet_cols_at(
        &path, &["x".to_string(), "g".to_string()]).expect("read");
    let rb2 = match v {
        Value::ArrowTable(t) => t,
        other => panic!("expected ArrowTable, got {other:?}"),
    };
    assert_eq!(rb2.num_rows(), 3);
    let schema = rb2.schema();
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    assert_eq!(names, vec!["x", "g"], "projection pushdown must drop `y`");
}

#[test]
fn parquet_read_cols_missing_column_is_err() {
    let rb = small_batch();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");
    lex_runtime::arrow::write_parquet_at(&rb, &path).unwrap();

    let r = lex_runtime::arrow::read_parquet_cols_at(&path, &["nope".to_string()]);
    assert!(r.is_err(), "expected Err for missing column, got {r:?}");
    let msg = r.err().unwrap();
    assert!(msg.contains("nope"), "error should name the missing column: {msg}");
}

#[test]
fn write_csv_round_trips_through_read_csv() {
    let rb = small_batch();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.csv");

    lex_runtime::arrow::write_csv_at(&rb, &path).expect("write csv");
    assert!(path.exists());

    let v = lex_runtime::arrow::read_csv_at(&path).expect("read csv");
    let rb2 = match v {
        Value::ArrowTable(t) => t,
        other => panic!("expected ArrowTable, got {other:?}"),
    };
    assert_eq!(rb2.num_rows(), 3);
    let r = lex_runtime::arrow::dispatch(
        "col_sum_int", &[Value::ArrowTable(rb2), Value::Str("x".into())])
        .unwrap().unwrap();
    assert_eq!(unwrap_ok(r), Value::Int(6));
}

/// End-to-end: `arrow.write_parquet` + `arrow.read_parquet` through the
/// VM with `[fs_read]` + `[fs_write]` scoped to a tempdir. Mirrors the
/// `arrow_read_csv_end_to_end` test, plus the write side.
#[test]
fn parquet_round_trip_via_vm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.parquet");

    // Build a tiny table via `arrow.from_int_columns`, write it,
    // read it back, sum the column. End-to-end through the VM.
    let src = r#"
import "std.list"  as list
import "std.arrow" as arrow

fn build_and_write(p :: Str) -> [fs_write] Int {
  let xs := list.cons(1, list.cons(2, list.cons(3, list.cons(4, list.cons(5, [])))))
  match arrow.from_int_columns(list.cons(("x", xs), [])) {
    Err(_) => -1,
    Ok(t)  => match arrow.write_parquet(t, p) {
      Ok(_)  => 0,
      Err(_) => -2,
    },
  }
}

fn sum_x(p :: Str) -> [fs_read] Int {
  match arrow.read_parquet(p) {
    Err(_) => -1,
    Ok(t) => match arrow.col_sum_int(t, "x") {
      Ok(s)  => s,
      Err(_) => -2,
    },
  }
}
"#;
    let mut policy = Policy::pure();
    policy.allow_effects.insert("fs_read".into());
    policy.allow_effects.insert("fs_write".into());
    policy.allow_fs_read.push(dir.path().to_path_buf());
    policy.allow_fs_write.push(dir.path().to_path_buf());

    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));

    let wrote = vm.call(
        "build_and_write",
        vec![Value::Str(path.to_string_lossy().to_string().into())],
    ).unwrap();
    assert_eq!(wrote, Value::Int(0), "write side should return 0");
    assert!(path.exists(), "parquet file should exist after write");

    let read = vm.call(
        "sum_x",
        vec![Value::Str(path.to_string_lossy().to_string().into())],
    ).unwrap();
    assert_eq!(read, Value::Int(15), "1+2+3+4+5 = 15");
}

/// `arrow.write_parquet` outside the `--allow-fs-write` scope must
/// surface as Err (not panic, not write the file).
#[test]
fn parquet_write_outside_allow_list_is_err() {
    let src = r#"
import "std.list"  as list
import "std.arrow" as arrow
fn go(p :: Str) -> [fs_write] Int {
  let xs := list.cons(1, list.cons(2, []))
  match arrow.from_int_columns(list.cons(("x", xs), [])) {
    Err(_) => -1,
    Ok(t)  => match arrow.write_parquet(t, p) {
      Ok(_)  => 0,
      Err(_) => -2,
    },
  }
}
"#;
    let mut policy = Policy::pure();
    policy.allow_effects.insert("fs_write".into());
    policy.allow_fs_write.push("/nonexistent/path".into());

    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let _ = lex_types::check_program(&stages);
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));

    // Trying to write to /tmp/refused.parquet must come back as Err(-2),
    // not as a panic or a successful write outside the allowlist.
    let r = vm.call(
        "go",
        vec![Value::Str("/tmp/refused.parquet".into())],
    ).unwrap();
    assert_eq!(r, Value::Int(-2), "write should be refused by policy");
    assert!(!std::path::Path::new("/tmp/refused.parquet").exists(),
        "refused write must not have created the file");
}
