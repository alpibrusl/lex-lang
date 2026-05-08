//! `lex op show` and `lex op log`.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Result};
use lex_store::Store;
use lex_vcs::{OpLog, OperationRecord};
use std::path::PathBuf;

fn parse_store(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--store" {
            if let Some(p) = it.next() { root = Some(PathBuf::from(p)); }
        } else {
            rest.push(a.clone());
        }
    }
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    (root, rest)
}

pub fn cmd_op(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex op {{show|log}} [--store DIR] ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "show" => cmd_op_show(fmt, rest),
        "log"  => cmd_op_log(fmt, rest),
        other  => bail!("unknown `lex op` subcommand: {other}"),
    }
}

fn cmd_op_show(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let op_id = rest.first().ok_or_else(|| anyhow!(
        "usage: lex op show [--store DIR] <op_id>"))?;
    let log = OpLog::open(&root)?;
    let rec = log.get(op_id)?
        .ok_or_else(|| anyhow!("unknown op_id: {op_id}"))?;
    let data = serde_json::json!({ "op": serde_json::to_value(&rec)? });
    acli::emit_or_text("op", data, fmt, || render_record(&rec));
    Ok(())
}

fn cmd_op_log(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut branch: Option<String> = None;
    let mut limit: Option<usize> = None;
    // `--budget-drift [PCT]` (#247) filters the log to ops whose
    // declared budget cost grew (or shrank) by at least PCT
    // percent. Default 10%. Bare flag = filter on; flag with value
    // overrides the threshold.
    let mut budget_drift: Option<f64> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else if a == "--limit" {
            limit = Some(it.next().ok_or_else(|| anyhow!("--limit needs N"))?
                .parse().map_err(|e| anyhow!("--limit: {e}"))?);
        } else if a == "--budget-drift" {
            // Optional numeric arg; if the next token isn't a
            // number, treat it as the next flag and use the
            // 10% default.
            let pct = it.clone().next()
                .and_then(|s| s.parse::<f64>().ok());
            if let Some(v) = pct {
                budget_drift = Some(v);
                it.next();
            } else {
                budget_drift = Some(10.0);
            }
        }
    }
    let store = Store::open(&root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    let head = store.get_branch(&branch)?
        .and_then(|b| b.head_op);
    let log = OpLog::open(&root)?;
    let recs = match head {
        Some(h) => log.walk_back(&h, limit)?,
        None => Vec::new(),
    };
    // Apply the budget-drift filter if requested. We don't feed
    // it through `walk_back`'s `limit` because limit operates on
    // the unfiltered DAG (it's a cost cap, not a result count);
    // filtering here gives the user "the most recent N
    // budget-drift events," which is what `--limit` should mean.
    let recs: Vec<OperationRecord> = match budget_drift {
        None => recs,
        Some(threshold_pct) => recs
            .into_iter()
            .filter(|r| budget_drift_pct(&r.op.kind)
                .map(|p| p.abs() >= threshold_pct)
                .unwrap_or(false))
            .collect(),
    };
    let arr: Vec<serde_json::Value> = recs.iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap();
            if let Some(pct) = budget_drift_pct(&r.op.kind) {
                v["budget_drift_pct"] = serde_json::json!(pct);
            }
            v
        }).collect();
    let data = serde_json::json!({
        "log": arr,
        "branch": branch,
        "budget_drift_threshold_pct": budget_drift,
    });
    acli::emit_or_text("op", data, fmt, || {
        for r in &recs { render_record(r); }
    });
    Ok(())
}

fn render_record(r: &OperationRecord) {
    println!("op_id:   {}", r.op_id);
    let kind_label = serde_json::to_value(&r.op.kind).ok()
        .and_then(|v| v.get("op").and_then(|s| s.as_str().map(str::to_string)))
        .unwrap_or_else(|| "?".into());
    println!("kind:    {kind_label}");
    if r.op.parents.is_empty() {
        println!("parents: (none)");
    } else {
        for p in &r.op.parents {
            println!("parent:  {p}");
        }
    }
    // #247: render the budget delta when the op carries one.
    let (from, to) = r.op.kind.budget_delta();
    if let Some(line) = render_budget_delta(from, to) {
        println!("cost:    {line}");
    }
    println!();
}

/// Render `cost:` line content for an op with budget. Examples:
///   `(unset)` if both sides are None
///   `→ 50` for an Add (no prior)
///   `100 → 50 (-50%)` for a shrink
///   `100 → 100 (no change)` for an unchanged budget
/// Returns `None` if there's nothing to render.
fn render_budget_delta(from: Option<u64>, to: Option<u64>) -> Option<String> {
    match (from, to) {
        (None, None) => None,
        (None, Some(t)) => Some(format!("→ {t}")),
        (Some(f), None) => Some(format!("{f} → (unset)")),
        (Some(f), Some(t)) if f == t => Some(format!("{f} → {t} (no change)")),
        (Some(f), Some(t)) => {
            let delta_pct = budget_pct(f, t);
            let sign = if t >= f { "+" } else { "" };
            Some(format!("{f} → {t} ({sign}{delta_pct:.1}%)"))
        }
    }
}

/// Signed percent change `(to - from) / from * 100`. Used by
/// `--budget-drift` filter and `cost:` line renderer. Anchors at
/// 0% when from == 0 (avoid div-by-zero — any non-zero `to` is
/// "infinite drift" by convention; treat it as +100% for the
/// filter so default --budget-drift 10 catches it).
pub(crate) fn budget_pct(from: u64, to: u64) -> f64 {
    if from == 0 {
        return if to == 0 { 0.0 } else { 100.0 };
    }
    let delta = to as f64 - from as f64;
    delta / from as f64 * 100.0
}

/// Signed budget drift percent for an op kind, if it carries
/// budget on both sides. Returns `None` when either side is
/// unset (an `AddFunction` reads as drift = +100% only when its
/// `budget_cost` is non-zero; we treat None-from as "no drift
/// signal" so newly-added budgets don't dominate the filter).
pub(crate) fn budget_drift_pct(kind: &lex_vcs::OperationKind) -> Option<f64> {
    let (from, to) = kind.budget_delta();
    match (from, to) {
        (Some(f), Some(t)) => Some(budget_pct(f, t)),
        _ => None,
    }
}
