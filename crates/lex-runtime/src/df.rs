//! `std.df` — Polars-backed query ops over `arrow.Table` (#427).
//!
//! The companion to `std.arrow`. Where `std.arrow` covers construction +
//! column reductions, `std.df` covers the query-shaped operations —
//! `filter`, `sort`, `group_by + agg`, `join` — that Polars already
//! does vectorised + parallel. Same input/output type (`Value::ArrowTable`);
//! the Polars `DataFrame` is internal plumbing.
//!
//! Conversion across the arrow-rs ↔ polars-arrow boundary is a
//! column-by-column copy (typed buffer → `Vec<T>` → `Series`). For
//! primitive columns this is a `memcpy`-speed walk; for `String`
//! columns it copies the offsets + bytes. On the scale `lex-frame`
//! cares about (≤ 10M rows) this is ~10 ms each direction, negligible
//! compared to the savings on the actual query.

use arrow_array::{
    Array, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType as ArrowDt, Field, Schema};
use lex_bytecode::Value;
use polars::prelude::{
    col, lit, Column, DataFrame, DataType as PlDt, Expr, IntoLazy, JoinArgs,
    JoinType, NamedFrom, PlSmallStr, Series, SortMultipleOptions,
};
use polars::prelude::IntoColumn;
use std::collections::VecDeque;
use std::sync::Arc;

// ---------- helpers ----------

fn err<T>(s: impl Into<String>) -> Result<T, String> { Err(s.into()) }

fn expect_table(v: Option<&Value>) -> Result<&Arc<RecordBatch>, String> {
    match v {
        Some(Value::ArrowTable(t)) => Ok(t),
        Some(other) => err(format!("df: expected arrow.Table, got {other:?}")),
        None => err("df: expected arrow.Table, got nothing"),
    }
}

fn expect_str(v: Option<&Value>) -> Result<&str, String> {
    match v {
        Some(Value::Str(s)) => Ok(s.as_str()),
        Some(other) => err(format!("df: expected Str, got {other:?}")),
        None => err("df: expected Str, got nothing"),
    }
}

fn expect_int(v: Option<&Value>) -> Result<i64, String> {
    match v {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => err(format!("df: expected Int, got {other:?}")),
        None => err("df: expected Int, got nothing"),
    }
}

fn expect_float(v: Option<&Value>) -> Result<f64, String> {
    match v {
        Some(Value::Float(f)) => Ok(*f),
        Some(Value::Int(n)) => Ok(*n as f64),
        Some(other) => err(format!("df: expected Float, got {other:?}")),
        None => err("df: expected Float, got nothing"),
    }
}

fn expect_bool(v: Option<&Value>) -> Result<bool, String> {
    match v {
        Some(Value::Bool(b)) => Ok(*b),
        Some(other) => err(format!("df: expected Bool, got {other:?}")),
        None => err("df: expected Bool, got nothing"),
    }
}

fn expect_list(v: Option<&Value>) -> Result<&VecDeque<Value>, String> {
    match v {
        Some(Value::List(items)) => Ok(items),
        Some(other) => err(format!("df: expected List, got {other:?}")),
        None => err("df: expected List, got nothing"),
    }
}

// ---------- conversion: arrow-rs RecordBatch ↔ polars DataFrame ----------

/// Build a Polars `DataFrame` from an arrow-rs `RecordBatch`. Each
/// column is copied through `Vec<Option<T>>` so nulls survive the
/// round-trip (otherwise `df.filter_isnull` would never see them);
/// cost is O(rows) memcpy-speed.
fn to_polars(rb: &RecordBatch) -> Result<DataFrame, String> {
    let mut cols: Vec<Column> = Vec::with_capacity(rb.num_columns());
    for (idx, field) in rb.schema().fields().iter().enumerate() {
        let name = field.name();
        let arr = rb.column(idx);
        let s = match arr.data_type() {
            ArrowDt::Int64 => {
                let a = arr.as_any().downcast_ref::<Int64Array>().unwrap();
                let buf: Vec<Option<i64>> = (0..a.len()).map(|i|
                    if a.is_null(i) { None } else { Some(a.value(i)) }
                ).collect();
                Series::new(PlSmallStr::from_str(name), buf)
            }
            ArrowDt::Float64 => {
                let a = arr.as_any().downcast_ref::<Float64Array>().unwrap();
                let buf: Vec<Option<f64>> = (0..a.len()).map(|i|
                    if a.is_null(i) { None } else { Some(a.value(i)) }
                ).collect();
                Series::new(PlSmallStr::from_str(name), buf)
            }
            ArrowDt::Utf8 => {
                let a = arr.as_any().downcast_ref::<StringArray>().unwrap();
                let buf: Vec<Option<&str>> = (0..a.len()).map(|i|
                    if a.is_null(i) { None } else { Some(a.value(i)) }
                ).collect();
                Series::new(PlSmallStr::from_str(name), buf)
            }
            other => return err(format!(
                "df: column `{name}` has unsupported type {other:?} (v1: Int64/Float64/Utf8)")),
        };
        cols.push(s.into());
    }
    // polars 0.53 switched `DataFrame::new(cols)` to take an explicit
    // height as the first arg; `new_infer_height` does what the v0.50
    // `new` did, deriving height from the first column.
    DataFrame::new_infer_height(cols).map_err(|e| format!("df: build DataFrame: {e}"))
}

/// Build an arrow-rs `RecordBatch` from a Polars `DataFrame`. Inverse
/// of `to_polars`, same O(rows) copy cost per column. Nulls are
/// preserved — output fields are emitted with `nullable=true` so the
/// arrow schema reflects what the polars-side filter / agg may have
/// produced.
fn from_polars(df: &DataFrame) -> Result<RecordBatch, String> {
    let mut fields: Vec<Field> = Vec::with_capacity(df.width());
    let mut arrays: Vec<arrow_array::ArrayRef> = Vec::with_capacity(df.width());
    for column in df.columns() {
        let name = column.name().as_str();
        let s = column.as_materialized_series();
        let (field, array): (Field, arrow_array::ArrayRef) = match s.dtype() {
            PlDt::Int64 => {
                let v: Vec<Option<i64>> = s.i64()
                    .map_err(|e| format!("df: column `{name}` as i64: {e}"))?
                    .iter().collect();
                (
                    Field::new(name, ArrowDt::Int64, true),
                    Arc::new(Int64Array::from(v)),
                )
            }
            PlDt::Float64 => {
                let v: Vec<Option<f64>> = s.f64()
                    .map_err(|e| format!("df: column `{name}` as f64: {e}"))?
                    .iter().collect();
                (
                    Field::new(name, ArrowDt::Float64, true),
                    Arc::new(Float64Array::from(v)),
                )
            }
            PlDt::String => {
                let v: Vec<Option<String>> = s.str()
                    .map_err(|e| format!("df: column `{name}` as Utf8: {e}"))?
                    .iter().map(|x| x.map(|s| s.to_string())).collect();
                (
                    Field::new(name, ArrowDt::Utf8, true),
                    Arc::new(StringArray::from(v)),
                )
            }
            // UInt32 surfaces from `count` aggregations in Polars.
            // Width promotes to Int64 (lex `Int` is 64-bit).
            PlDt::UInt32 => {
                let v: Vec<Option<i64>> = s.u32()
                    .map_err(|e| format!("df: column `{name}` as u32: {e}"))?
                    .iter().map(|x| x.map(|n| n as i64)).collect();
                (
                    Field::new(name, ArrowDt::Int64, true),
                    Arc::new(Int64Array::from(v)),
                )
            }
            other => return err(format!(
                "df: polars column `{name}` has unsupported type {other:?}")),
        };
        fields.push(field);
        arrays.push(array);
    }
    let schema = Arc::new(Schema::new(fields));
    RecordBatch::try_new(schema, arrays)
        .map_err(|e| format!("df: RecordBatch::try_new: {e}"))
}

// ---------- ops ----------

fn pack(df: DataFrame) -> Result<Value, String> {
    let rb = from_polars(&df)?;
    Ok(Value::ArrowTable(Arc::new(rb)))
}

fn filter_eq_int(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_int(args.get(2))?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).eq(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_eq_int: {e}"))?;
    pack(out)
}

fn filter_gt_int(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_int(args.get(2))?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).gt(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_gt_int: {e}"))?;
    pack(out)
}

fn filter_lt_int(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_int(args.get(2))?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).lt(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_lt_int: {e}"))?;
    pack(out)
}

/// Type-check `col_name` against `wanted` before letting Polars run.
/// The polars error for a type-mismatched filter is opaque
/// ("cannot compare Int64 with Utf8"); this lets us return a stable
/// shape like "expected utf8 column, got int64". Caller passes the
/// `RecordBatch` we'll convert to polars, so we use the arrow schema
/// (which is what an agent saw via `arrow.col_type`).
fn expect_col_type(rb: &RecordBatch, col_name: &str, wanted: ArrowDt, op: &str) -> Result<(), String> {
    let schema = rb.schema();
    let (_, field) = schema
        .column_with_name(col_name)
        .ok_or_else(|| format!("df.{op}: column `{col_name}` not found"))?;
    if field.data_type() != &wanted {
        return err(format!(
            "df.{op}: expected {wanted:?} column, got {:?}",
            field.data_type()
        ));
    }
    Ok(())
}

fn filter_eq_str(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_str(args.get(2))?;
    expect_col_type(rb, col_name, ArrowDt::Utf8, "filter_eq_str")?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).eq(lit(needle.to_string())))
        .collect()
        .map_err(|e| format!("df.filter_eq_str: {e}"))?;
    pack(out)
}

fn filter_in_str(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needles_list = expect_list(args.get(2))?;
    expect_col_type(rb, col_name, ArrowDt::Utf8, "filter_in_str")?;
    let mut needles: Vec<String> = Vec::with_capacity(needles_list.len());
    for v in needles_list {
        match v {
            Value::Str(s) => needles.push(s.to_string()),
            other => return err(format!(
                "df.filter_in_str: needle list contained non-Str: {other:?}")),
        }
    }
    // Empty needle list → empty result (SQL `IN ()` is false).
    if needles.is_empty() {
        // Build an empty version of `rb` using its existing schema —
        // saves the round-trip through polars for a degenerate input.
        let empty = RecordBatch::new_empty(rb.schema());
        return Ok(Value::ArrowTable(Arc::new(empty)));
    }
    let df = to_polars(rb)?;
    let needle_series: Series =
        Series::new(PlSmallStr::from_static("__in"), needles).into_column().take_materialized_series();
    let out = df.lazy()
        .filter(col(col_name).is_in(lit(needle_series), false))
        .collect()
        .map_err(|e| format!("df.filter_in_str: {e}"))?;
    pack(out)
}

fn filter_eq_float(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_float(args.get(2))?;
    expect_col_type(rb, col_name, ArrowDt::Float64, "filter_eq_float")?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).eq(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_eq_float: {e}"))?;
    pack(out)
}

fn filter_lt_float(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_float(args.get(2))?;
    expect_col_type(rb, col_name, ArrowDt::Float64, "filter_lt_float")?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).lt(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_lt_float: {e}"))?;
    pack(out)
}

fn filter_gt_float(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let needle = expect_float(args.get(2))?;
    expect_col_type(rb, col_name, ArrowDt::Float64, "filter_gt_float")?;
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).gt(lit(needle)))
        .collect()
        .map_err(|e| format!("df.filter_gt_float: {e}"))?;
    pack(out)
}

fn filter_isnull(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    // Type-agnostic — works on any column. Just verify the column exists.
    if rb.schema().column_with_name(col_name).is_none() {
        return err(format!("df.filter_isnull: column `{col_name}` not found"));
    }
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).is_null())
        .collect()
        .map_err(|e| format!("df.filter_isnull: {e}"))?;
    pack(out)
}

fn filter_notnull(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    if rb.schema().column_with_name(col_name).is_none() {
        return err(format!("df.filter_notnull: column `{col_name}` not found"));
    }
    let df = to_polars(rb)?;
    let out = df.lazy()
        .filter(col(col_name).is_not_null())
        .collect()
        .map_err(|e| format!("df.filter_notnull: {e}"))?;
    pack(out)
}

fn drop_nulls(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let cols_list = expect_list(args.get(1))?;
    // Empty list → no-op (return the input unchanged).
    if cols_list.is_empty() {
        return Ok(Value::ArrowTable(Arc::clone(rb)));
    }
    let mut cols: Vec<String> = Vec::with_capacity(cols_list.len());
    {
        let schema = rb.schema();
        for v in cols_list {
            match v {
                Value::Str(s) => {
                    if schema.column_with_name(s.as_str()).is_none() {
                        return err(format!("df.drop_nulls: column `{s}` not found"));
                    }
                    cols.push(s.to_string());
                }
                other => return err(format!(
                    "df.drop_nulls: column list contained non-Str: {other:?}")),
            }
        }
    }
    let df = to_polars(rb)?;
    let out = df
        .drop_nulls(Some(&cols))
        .map_err(|e| format!("df.drop_nulls: {e}"))?;
    pack(out)
}

fn sort_by(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let col_name = expect_str(args.get(1))?;
    let asc = expect_bool(args.get(2))?;
    let df = to_polars(rb)?;
    let mut sort_opts = SortMultipleOptions::default();
    sort_opts = sort_opts.with_order_descending(!asc);
    let out = df.lazy()
        .sort([col_name], sort_opts)
        .collect()
        .map_err(|e| format!("df.sort_by: {e}"))?;
    pack(out)
}

/// `df.group_by_agg(t, keys, specs)`. `keys :: List[Str]`. Each spec is
/// `(out_name :: Str, in_name :: Str, op :: Str)` where op ∈ "sum" |
/// "mean" | "min" | "max" | "count" | "n_distinct".
fn group_by_agg(args: &[Value]) -> Result<Value, String> {
    let rb = expect_table(args.first())?;
    let keys_list = expect_list(args.get(1))?;
    let specs_list = expect_list(args.get(2))?;

    let mut keys: Vec<&str> = Vec::with_capacity(keys_list.len());
    for k in keys_list {
        let s = match k {
            Value::Str(s) => s.as_str(),
            other => return err(format!("group_by_agg: key list contained non-Str: {other:?}")),
        };
        keys.push(s);
    }

    let mut aggs: Vec<Expr> = Vec::with_capacity(specs_list.len());
    for spec in specs_list {
        let t = match spec {
            Value::Tuple(t) if t.len() == 3 => t,
            other => return err(format!(
                "group_by_agg: spec must be (out, in, op) tuple, got {other:?}")),
        };
        let out_name = match &t[0] {
            Value::Str(s) => s.as_str(),
            other => return err(format!("group_by_agg: out_name not Str: {other:?}")),
        };
        let in_name = match &t[1] {
            Value::Str(s) => s.as_str(),
            other => return err(format!("group_by_agg: in_name not Str: {other:?}")),
        };
        let op = match &t[2] {
            Value::Str(s) => s.as_str(),
            other => return err(format!("group_by_agg: op not Str: {other:?}")),
        };
        let e = match op {
            "sum"        => col(in_name).sum().alias(out_name),
            "mean"       => col(in_name).mean().alias(out_name),
            "min"        => col(in_name).min().alias(out_name),
            "max"        => col(in_name).max().alias(out_name),
            "count"      => col(in_name).count().alias(out_name),
            "n_distinct" => col(in_name).n_unique().alias(out_name),
            other => return err(format!(
                "group_by_agg: unknown op `{other}` (v1: sum|mean|min|max|count|n_distinct)")),
        };
        aggs.push(e);
    }

    let df = to_polars(rb)?;
    let out = df.lazy()
        .group_by(keys.iter().map(|k| col(*k)).collect::<Vec<_>>())
        .agg(aggs)
        .collect()
        .map_err(|e| format!("df.group_by_agg: {e}"))?;
    pack(out)
}

fn inner_join(args: &[Value]) -> Result<Value, String> {
    let lhs = expect_table(args.first())?;
    let rhs = expect_table(args.get(1))?;
    let on = expect_str(args.get(2))?;
    let l = to_polars(lhs)?;
    let r = to_polars(rhs)?;
    let out = l.lazy()
        .join(r.lazy(), [col(on)], [col(on)], JoinArgs::new(JoinType::Inner))
        .collect()
        .map_err(|e| format!("df.inner_join: {e}"))?;
    pack(out)
}

fn left_join(args: &[Value]) -> Result<Value, String> {
    let lhs = expect_table(args.first())?;
    let rhs = expect_table(args.get(1))?;
    let on = expect_str(args.get(2))?;
    let l = to_polars(lhs)?;
    let r = to_polars(rhs)?;
    let out = l.lazy()
        .join(r.lazy(), [col(on)], [col(on)], JoinArgs::new(JoinType::Left))
        .collect()
        .map_err(|e| format!("df.left_join: {e}"))?;
    pack(out)
}

// ---------- helpers (mirror arrow.rs) ----------

fn ok(v: Value) -> Value {
    Value::Variant { name: "Ok".into(), args: vec![v] }
}

fn err_variant(s: String) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(s.into())] }
}

fn lift_result(r: Result<Value, String>) -> Result<Value, String> {
    match r {
        Ok(v)  => Ok(ok(v)),
        Err(s) => Ok(err_variant(s)),
    }
}

// ---------- public dispatch ----------

pub fn dispatch(op: &str, args: &[Value]) -> Option<Result<Value, String>> {
    Some(match op {
        "filter_eq_int"   => lift_result(filter_eq_int(args)),
        "filter_gt_int"   => lift_result(filter_gt_int(args)),
        "filter_lt_int"   => lift_result(filter_lt_int(args)),
        // #433 — string/float/null filter predicates.
        "filter_eq_str"   => lift_result(filter_eq_str(args)),
        "filter_in_str"   => lift_result(filter_in_str(args)),
        "filter_eq_float" => lift_result(filter_eq_float(args)),
        "filter_lt_float" => lift_result(filter_lt_float(args)),
        "filter_gt_float" => lift_result(filter_gt_float(args)),
        "filter_isnull"   => lift_result(filter_isnull(args)),
        "filter_notnull"  => lift_result(filter_notnull(args)),
        "drop_nulls"      => lift_result(drop_nulls(args)),
        "sort_by"         => lift_result(sort_by(args)),
        "group_by_agg"    => lift_result(group_by_agg(args)),
        "inner_join"      => lift_result(inner_join(args)),
        "left_join"       => lift_result(left_join(args)),
        _ => return None,
    })
}
