//! `std.list` implementations (#778). The higher-order functions
//! (`map`, `par_map`, `sort_by`, `filter`, `fold`) are lowered by the
//! compiler to native VM ops and have no entry here; the catalogue marks
//! them `VmNative`.
//!
//! Arguments arrive by value, so builtins that return a modified list
//! move the input (copy-on-write makes that O(1) when the list is
//! uniquely owned, #774) instead of cloning every element.

use super::Entry;
use crate::builtins::{expect_int, expect_list, none, some};
use lex_bytecode::{List, Value};

pub(crate) const TABLE: &[Entry] = &[
    ("len", len),
    ("is_empty", is_empty),
    ("range", range),
    ("head", head),
    ("tail", tail),
    ("concat", concat),
    ("reverse", reverse),
    ("cons", cons),
    ("enumerate", enumerate),
];

fn owned_list(v: Option<Value>) -> Result<List, String> {
    match v {
        Some(Value::List(xs)) => Ok(xs),
        Some(other) => Err(format!("expected List, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn len(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Int(expect_list(args.first())?.len() as i64))
}

fn is_empty(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Bool(expect_list(args.first())?.is_empty()))
}

fn head(args: Vec<Value>) -> Result<Value, String> {
    let xs = expect_list(args.first())?;
    Ok(match xs.front() {
        Some(v) => some(v.clone()),
        None => none(),
    })
}

/// Everything but the first element. Moves the list and pops its
/// front, so a uniquely owned list is not copied.
fn tail(args: Vec<Value>) -> Result<Value, String> {
    let mut xs = owned_list(args.into_iter().next())?;
    xs.pop_front();
    Ok(Value::List(xs))
}

fn range(args: Vec<Value>) -> Result<Value, String> {
    let lo = expect_int(args.first())?;
    let hi = expect_int(args.get(1))?;
    Ok(Value::List((lo..hi).map(Value::Int).collect::<std::collections::VecDeque<_>>().into()))
}

fn concat(args: Vec<Value>) -> Result<Value, String> {
    let mut it = args.into_iter();
    let mut out = owned_list(it.next())?;
    let rest = owned_list(it.next())?;
    out.extend(rest.iter().cloned());
    Ok(Value::List(out))
}

fn reverse(args: Vec<Value>) -> Result<Value, String> {
    let xs = owned_list(args.into_iter().next())?;
    let rev: std::collections::VecDeque<Value> = xs.into_inner().into_iter().rev().collect();
    Ok(Value::List(rev.into()))
}

/// Prepend one element (#334). The single implementation of `list.cons`:
/// the by-value fast path and the borrowed path both land here, so the
/// two can no longer drift (#774, #778). A missing tail is an empty
/// list, as the fast path always treated it.
fn cons(args: Vec<Value>) -> Result<Value, String> {
    let mut it = args.into_iter();
    let head = it.next().unwrap_or(Value::Unit);
    let mut tail = match it.next() {
        Some(Value::List(v)) => v,
        Some(other) => return Err(format!("list.cons: expected List, got {other:?}")),
        None => List::new(),
    };
    tail.push_front(head);
    Ok(Value::List(tail))
}

fn enumerate(args: Vec<Value>) -> Result<Value, String> {
    let xs = expect_list(args.first())?;
    let pairs = xs
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, v)| Value::Tuple(vec![Value::Int(i as i64), v]))
        .collect::<std::collections::VecDeque<_>>();
    Ok(Value::List(pairs.into()))
}
