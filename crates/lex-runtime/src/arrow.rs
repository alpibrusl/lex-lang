//! `std.arrow` — Apache Arrow `RecordBatch` as a first-class `Value`.
//!
//! This module is the runtime side of #426. Construction builtins
//! (`arrow.from_int_columns`, …) take Lex `List[Int]` / `List[Float]` /
//! `List[Str]` columns and pack them into a flat `RecordBatch`; numeric
//! reductions (`arrow.col_sum_int`, `arrow.col_mean`, …) run as a single
//! Rust call over the underlying buffer, bypassing the bytecode VM for
//! the inner loop.
//!
//! The only `Value` shape leaving this module that touches the Arrow
//! dependency is `Value::ArrowTable(Arc<RecordBatch>)`. Everything else
//! is plain Lex values.

use arrow_array::{
    Array, ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use lex_bytecode::Value;
use std::collections::VecDeque;
use std::sync::Arc;

// ---------- helpers ----------

fn err<T>(s: impl Into<String>) -> Result<T, String> { Err(s.into()) }

fn expect_table(v: Option<&Value>) -> Result<&Arc<RecordBatch>, String> {
    match v {
        Some(Value::ArrowTable(t)) => Ok(t),
        Some(other) => err(format!("expected arrow.Table, got {other:?}")),
        None => err("expected arrow.Table, got nothing"),
    }
}

fn expect_str(v: Option<&Value>) -> Result<&str, String> {
    match v {
        Some(Value::Str(s)) => Ok(s.as_str()),
        Some(other) => err(format!("expected Str, got {other:?}")),
        None => err("expected Str, got nothing"),
    }
}

fn expect_int(v: Option<&Value>) -> Result<i64, String> {
    match v {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => err(format!("expected Int, got {other:?}")),
        None => err("expected Int, got nothing"),
    }
}

fn expect_list(v: Option<&Value>) -> Result<&VecDeque<Value>, String> {
    match v {
        Some(Value::List(items)) => Ok(items),
        Some(other) => err(format!("expected List, got {other:?}")),
        None => err("expected List, got nothing"),
    }
}

/// Decode `List[(Str, List[T])]` shape used by all `from_*_columns`
/// constructors. Returns `Vec<(name, values_list)>`.
fn decode_columns_list<'a>(
    list: &'a VecDeque<Value>,
) -> Result<Vec<(&'a str, &'a VecDeque<Value>)>, String> {
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let pair = match item {
            Value::Tuple(t) if t.len() == 2 => t,
            other => return err(format!(
                "from_*_columns: column #{i} must be a (Str, List) tuple, got {other:?}")),
        };
        let name = match &pair[0] {
            Value::Str(s) => s.as_str(),
            other => return err(format!(
                "from_*_columns: column #{i} name must be Str, got {other:?}")),
        };
        let values = match &pair[1] {
            Value::List(items) => items,
            other => return err(format!(
                "from_*_columns: column #{i} (`{name}`) values must be List, got {other:?}")),
        };
        out.push((name, values));
    }
    Ok(out)
}

fn build_schema_and_check_lengths(
    cols: &[(&str, ArrayRef)],
) -> Result<Schema, String> {
    if cols.is_empty() {
        return Ok(Schema::empty());
    }
    let nrows = cols[0].1.len();
    let mut fields = Vec::with_capacity(cols.len());
    for (name, arr) in cols {
        if arr.len() != nrows {
            return err(format!(
                "from_*_columns: column `{name}` has {} rows, expected {nrows}",
                arr.len()));
        }
        fields.push(Field::new(*name, arr.data_type().clone(), false));
    }
    Ok(Schema::new(fields))
}

fn pack_table(cols: Vec<(&str, ArrayRef)>) -> Result<Value, String> {
    let schema = build_schema_and_check_lengths(&cols)?;
    let arrays: Vec<ArrayRef> = cols.into_iter().map(|(_, a)| a).collect();
    let batch = RecordBatch::try_new(Arc::new(schema), arrays)
        .map_err(|e| format!("arrow: failed to build RecordBatch: {e}"))?;
    Ok(Value::ArrowTable(Arc::new(batch)))
}

// ---------- constructors ----------

/// `arrow.from_int_columns(List[(Str, List[Int])]) -> Result[Table, Str]`
fn from_int_columns(args: &[Value]) -> Result<Value, String> {
    let list = expect_list(args.first())?;
    let pairs = decode_columns_list(list)?;
    let mut owned_names: Vec<String> = Vec::with_capacity(pairs.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(pairs.len());
    for (name, values) in &pairs {
        owned_names.push((*name).to_string());
        let mut buf: Vec<i64> = Vec::with_capacity(values.len());
        for v in values.iter() {
            match v {
                Value::Int(n) => buf.push(*n),
                other => return err(format!(
                    "from_int_columns: column `{name}` non-Int element: {other:?}")),
            }
        }
        arrays.push(Arc::new(Int64Array::from(buf)) as ArrayRef);
    }
    let cols: Vec<(&str, ArrayRef)> = owned_names.iter().map(|n| n.as_str())
        .zip(arrays.into_iter()).collect();
    pack_table(cols)
}

/// `arrow.from_float_columns(List[(Str, List[Float])]) -> Result[Table, Str]`
fn from_float_columns(args: &[Value]) -> Result<Value, String> {
    let list = expect_list(args.first())?;
    let pairs = decode_columns_list(list)?;
    let mut owned_names: Vec<String> = Vec::with_capacity(pairs.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(pairs.len());
    for (name, values) in &pairs {
        owned_names.push((*name).to_string());
        let mut buf: Vec<f64> = Vec::with_capacity(values.len());
        for v in values.iter() {
            match v {
                Value::Float(f) => buf.push(*f),
                Value::Int(n) => buf.push(*n as f64),
                other => return err(format!(
                    "from_float_columns: column `{name}` non-Float element: {other:?}")),
            }
        }
        arrays.push(Arc::new(Float64Array::from(buf)) as ArrayRef);
    }
    let cols: Vec<(&str, ArrayRef)> = owned_names.iter().map(|n| n.as_str())
        .zip(arrays.into_iter()).collect();
    pack_table(cols)
}

/// `arrow.from_str_columns(List[(Str, List[Str])]) -> Result[Table, Str]`
fn from_str_columns(args: &[Value]) -> Result<Value, String> {
    let list = expect_list(args.first())?;
    let pairs = decode_columns_list(list)?;
    let mut owned_names: Vec<String> = Vec::with_capacity(pairs.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(pairs.len());
    for (name, values) in &pairs {
        owned_names.push((*name).to_string());
        let mut buf: Vec<String> = Vec::with_capacity(values.len());
        for v in values.iter() {
            match v {
                Value::Str(s) => buf.push(s.to_string()),
                other => return err(format!(
                    "from_str_columns: column `{name}` non-Str element: {other:?}")),
            }
        }
        arrays.push(Arc::new(StringArray::from(buf)) as ArrayRef);
    }
    let cols: Vec<(&str, ArrayRef)> = owned_names.iter().map(|n| n.as_str())
        .zip(arrays.into_iter()).collect();
    pack_table(cols)
}

// ---------- introspection ----------

fn nrows(args: &[Value]) -> Result<Value, String> {
    Ok(Value::Int(expect_table(args.first())?.num_rows() as i64))
}

fn ncols(args: &[Value]) -> Result<Value, String> {
    Ok(Value::Int(expect_table(args.first())?.num_columns() as i64))
}

fn col_names(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let names: VecDeque<Value> = t.schema().fields().iter()
        .map(|f| Value::Str(f.name().as_str().into()))
        .collect();
    Ok(Value::List(names))
}

fn col_type(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    match t.schema().column_with_name(name) {
        None => Ok(none()),
        Some((_, field)) => Ok(some(Value::Str(format!("{}", field.data_type()).into()))),
    }
}

// ---------- column reductions ----------

fn lookup_array<'a>(t: &'a RecordBatch, name: &str) -> Result<&'a ArrayRef, String> {
    let (idx, _) = t.schema().column_with_name(name)
        .ok_or_else(|| format!("arrow: column `{name}` not found"))?;
    Ok(t.column(idx))
}

fn as_int64<'a>(arr: &'a ArrayRef, name: &str) -> Result<&'a Int64Array, String> {
    arr.as_any().downcast_ref::<Int64Array>()
        .ok_or_else(|| format!(
            "arrow: column `{name}` is {}, not Int64",
            arr.data_type()))
}

fn as_float64<'a>(arr: &'a ArrayRef, name: &str) -> Result<&'a Float64Array, String> {
    arr.as_any().downcast_ref::<Float64Array>()
        .ok_or_else(|| format!(
            "arrow: column `{name}` is {}, not Float64",
            arr.data_type()))
}

fn col_sum_int(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = as_int64(lookup_array(t, name)?, name)?;
    let s: i64 = arrow_arith::aggregate::sum(arr).unwrap_or(0);
    Ok(Value::Int(s))
}

fn col_sum_float(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = lookup_array(t, name)?;
    let s = match arr.data_type() {
        DataType::Float64 => arrow_arith::aggregate::sum(as_float64(arr, name)?).unwrap_or(0.0),
        DataType::Int64 => arrow_arith::aggregate::sum(as_int64(arr, name)?).unwrap_or(0) as f64,
        other => return err(format!(
            "col_sum_float: column `{name}` is {other:?}, expected Int64 or Float64")),
    };
    Ok(Value::Float(s))
}

fn col_mean(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = lookup_array(t, name)?;
    let n = arr.len() as f64 - arr.null_count() as f64;
    if n == 0.0 { return Ok(none()); }
    let total: f64 = match arr.data_type() {
        DataType::Float64 => arrow_arith::aggregate::sum(as_float64(arr, name)?).unwrap_or(0.0),
        DataType::Int64 => arrow_arith::aggregate::sum(as_int64(arr, name)?).unwrap_or(0) as f64,
        other => return err(format!(
            "col_mean: column `{name}` is {other:?}, expected Int64 or Float64")),
    };
    Ok(some(Value::Float(total / n)))
}

fn col_min_int(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = as_int64(lookup_array(t, name)?, name)?;
    match arrow_arith::aggregate::min(arr) {
        Some(v) => Ok(some(Value::Int(v))),
        None => Ok(none()),
    }
}

fn col_max_int(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = as_int64(lookup_array(t, name)?, name)?;
    match arrow_arith::aggregate::max(arr) {
        Some(v) => Ok(some(Value::Int(v))),
        None => Ok(none()),
    }
}

fn col_count(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let name = expect_str(args.get(1))?;
    let arr = lookup_array(t, name)?;
    Ok(Value::Int((arr.len() - arr.null_count()) as i64))
}

// ---------- slicing ----------

fn head(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let n = expect_int(args.get(1))?.max(0) as usize;
    let take = n.min(t.num_rows());
    Ok(Value::ArrowTable(Arc::new(t.slice(0, take))))
}

fn tail(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let n = expect_int(args.get(1))?.max(0) as usize;
    let total = t.num_rows();
    let take = n.min(total);
    Ok(Value::ArrowTable(Arc::new(t.slice(total - take, take))))
}

fn slice(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let start = expect_int(args.get(1))?.max(0) as usize;
    let stop  = expect_int(args.get(2))?.max(0) as usize;
    let total = t.num_rows();
    let s = start.min(total);
    let e = stop.min(total).max(s);
    Ok(Value::ArrowTable(Arc::new(t.slice(s, e - s))))
}

fn select_cols(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let names_list = expect_list(args.get(1))?;
    let mut indices = Vec::with_capacity(names_list.len());
    for v in names_list.iter() {
        let n = match v {
            Value::Str(s) => s.as_str(),
            other => return err(format!(
                "select_cols: name list contained non-Str: {other:?}")),
        };
        let (i, _) = t.schema().column_with_name(n)
            .ok_or_else(|| format!("select_cols: column `{n}` not found"))?;
        indices.push(i);
    }
    let projected = t.project(&indices)
        .map_err(|e| format!("select_cols: {e}"))?;
    Ok(Value::ArrowTable(Arc::new(projected)))
}

fn drop_col(args: &[Value]) -> Result<Value, String> {
    let t = expect_table(args.first())?;
    let drop_name = expect_str(args.get(1))?;
    let mut keep = Vec::with_capacity(t.num_columns());
    for (i, f) in t.schema().fields().iter().enumerate() {
        if f.name() != drop_name { keep.push(i); }
    }
    if keep.len() == t.num_columns() {
        return err(format!("drop_col: column `{drop_name}` not found"));
    }
    let projected = t.project(&keep)
        .map_err(|e| format!("drop_col: {e}"))?;
    Ok(Value::ArrowTable(Arc::new(projected)))
}

// ---------- value-conversion helpers (mirror builtins.rs) ----------

fn some(v: Value) -> Value {
    Value::Variant { name: "Some".into(), args: vec![v] }
}

fn none() -> Value {
    Value::Variant { name: "None".into(), args: vec![] }
}

fn ok(v: Value) -> Value {
    Value::Variant { name: "Ok".into(), args: vec![v] }
}

fn err_variant(s: String) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(s.into())] }
}

/// Lift a kernel that returns `Result<Value, String>` into a Lex
/// `Result[T, Str]` Value: an inner `Err(s)` becomes `Err(Value::Str)`,
/// `Ok(v)` becomes `Ok(v)`. Wrap kernels whose Lex signature is
/// `Result[T, Str]` with this; raw kernels (e.g. `nrows -> Int`) stay
/// as `Result<Value, String>` and propagate the host error.
fn lift_result(r: Result<Value, String>) -> Result<Value, String> {
    match r {
        Ok(v)  => Ok(ok(v)),
        Err(s) => Ok(err_variant(s)),
    }
}

// ---------- public entry point ----------

/// Dispatch an `arrow.*` builtin call. Returns `Some(Result)` if the op
/// was recognised, `None` if it should fall through to other dispatch
/// (the caller treats `None` as "unknown op").
///
/// Kernels whose Lex signature is `Result[T, Str]` go through `lift_result`
/// so a host-side `Err(s)` becomes a Lex `Err("...")` Variant, not a
/// runtime panic. Kernels that return a bare type (`nrows :: Table -> Int`)
/// don't lift — a bad argument there *is* a programmer error and should
/// surface as a runtime mismatch.
pub fn dispatch(op: &str, args: &[Value]) -> Option<Result<Value, String>> {
    Some(match op {
        // -- Result-returning constructors / ops --
        "from_int_columns"   => lift_result(from_int_columns(args)),
        "from_float_columns" => lift_result(from_float_columns(args)),
        "from_str_columns"   => lift_result(from_str_columns(args)),
        "col_sum_int"        => lift_result(col_sum_int(args)),
        "col_sum_float"      => lift_result(col_sum_float(args)),
        "col_mean"           => lift_result(col_mean(args)),
        "col_min_int"        => lift_result(col_min_int(args)),
        "col_max_int"        => lift_result(col_max_int(args)),
        "col_count"          => lift_result(col_count(args)),
        "select_cols"        => lift_result(select_cols(args)),
        "drop_col"           => lift_result(drop_col(args)),
        // -- bare-return introspection / slicing --
        "nrows"              => nrows(args),
        "ncols"              => ncols(args),
        "col_names"          => col_names(args),
        "col_type"           => col_type(args),
        "head"               => head(args),
        "tail"               => tail(args),
        "slice"              => slice(args),
        _ => return None,
    })
}
