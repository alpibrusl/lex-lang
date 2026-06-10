//! `lex op show` and `lex op log`.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_store::Store;
use lex_vcs::{OpLog, OperationRecord};
use std::path::PathBuf;

/// Attach `Authorization: Bearer <token>` to a request when a token
/// is present. Falls back to the unmodified builder when absent so
/// unauthenticated `lex serve` remotes keep working (#630).
fn with_auth<B>(req: ureq::RequestBuilder<B>, token: Option<&str>) -> ureq::RequestBuilder<B> {
    match token {
        Some(t) => req.header("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

/// Resolve the Bearer token: `--token` flag > `LEXHUB_TOKEN` env var > None.
fn resolve_token(flag: Option<String>) -> Option<String> {
    flag.or_else(|| std::env::var("LEXHUB_TOKEN").ok().filter(|s| !s.is_empty()))
}

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
        "usage: lex op {{show|log|push|pull|repack|gc}} [--store DIR] ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "show"   => cmd_op_show(fmt, rest),
        "log"    => cmd_op_log(fmt, rest),
        "push"   => cmd_op_push(fmt, rest),
        "pull"   => cmd_op_pull(fmt, rest),
        "repack" => cmd_op_repack(fmt, rest),
        "gc"     => cmd_op_gc(fmt, rest),
        other    => bail!("unknown `lex op` subcommand: {other}"),
    }
}

/// `lex op gc {--dry-run|--confirm} [--retain JSON ...] [--store DIR]`
/// (#261 slice 2). Plans (or applies) a predicate-driven garbage
/// collection of the op log.
///
/// Retention rules combine: every op reachable from any branch
/// head is always kept; ops matching `--retain` predicates or
/// policy.json's `gc_retention.retain` entries are kept; every
/// parent of a retained op is kept transitively (DAG integrity).
fn cmd_op_gc(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut dry_run = false;
    let mut confirm = false;
    let mut cli_retain: Vec<lex_vcs::Predicate> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => dry_run = true,
            "--confirm" => confirm = true,
            "--retain" => {
                let raw = it.next()
                    .ok_or_else(|| anyhow!("--retain needs a JSON predicate"))?;
                let v: serde_json::Value = serde_json::from_str(raw)
                    .with_context(|| format!("parsing --retain JSON: {raw}"))?;
                let p = lex_vcs::Predicate::from_value(&v)
                    .map_err(|e| anyhow!("--retain predicate: {e}"))?;
                cli_retain.push(p);
            }
            other => bail!("unexpected arg `{other}` (usage: lex op gc \
                [--dry-run|--confirm] [--retain JSON]... [--store DIR])"),
        }
    }
    if !dry_run && !confirm {
        bail!("`lex op gc` requires either --dry-run or --confirm");
    }
    if dry_run && confirm {
        bail!("--dry-run and --confirm are mutually exclusive");
    }
    let store = Store::open(&root)?;
    let plan = store.plan_gc(&cli_retain)?;
    let removed = if confirm { store.apply_gc(&plan)? } else { 0 };
    let data = serde_json::json!({
        "store": root.display().to_string(),
        "dry_run": dry_run,
        "to_delete": &plan.to_delete,
        "retained_count": plan.retained.len(),
        "removed": removed,
    });
    acli::emit_or_text("op", data, fmt, || {
        let n = plan.to_delete.len();
        if dry_run {
            println!("plan: would delete {n} op(s); retain {} op(s)",
                plan.retained.len());
        } else if removed == 0 {
            println!("nothing to do (already at the retention boundary)");
        } else {
            println!("removed {removed} op(s); retained {} op(s)",
                plan.retained.len());
        }
    });
    Ok(())
}

/// `lex op repack [--threshold N] [--store DIR]` (#261 slice 1).
/// Consolidates loose `<op_id>.json` files into a deterministic,
/// content-addressed packfile. No-op when loose-file count is
/// below `--threshold` (default 1000) — small stores stay loose.
fn cmd_op_repack(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut threshold: usize = 1000;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--threshold" {
            threshold = it.next()
                .ok_or_else(|| anyhow!("--threshold needs N"))?
                .parse()
                .map_err(|e| anyhow!("--threshold: {e}"))?;
        } else {
            bail!("unexpected arg `{a}` (usage: lex op repack [--threshold N] [--store DIR])");
        }
    }
    let log = OpLog::open(&root)?;
    let packed = log.repack(threshold)?;
    let data = serde_json::json!({
        "packed": packed,
        "threshold": threshold,
        "store": root.display().to_string(),
    });
    acli::emit_or_text("op", data, fmt, || {
        if packed == 0 {
            println!("no repack: loose count below threshold ({threshold})");
        } else {
            println!("packed {packed} loose op(s) into a packfile");
        }
    });
    Ok(())
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

/// `lex op push <remote_url> [--branch NAME] [--since OP_ID]
/// [--dry-run] [--store DIR]` (#242).
///
/// Walks the local op log on `<branch>` (default: current
/// branch), computes the set of ops not yet on the remote, and
/// posts them to `<remote_url>/v1/ops/batch`.
///
/// Discovery: when `--since` is absent, the client probes
/// `<remote_url>/v1/branches/<branch>/head` for the remote's
/// current head_op and uses `OpLog::ops_since(local_head,
/// remote_head)` to compute the delta. With `--since OP_ID`, the
/// caller supplies the cutoff directly — useful when the remote
/// is offline or when pushing to a branch the remote doesn't
/// have yet (`--since` set to the genesis means "send all").
///
/// Idempotency: server-side `OpLog::put` is idempotent on
/// `op_id`, so re-pushing the same delta is safe and converges to
/// `added: 0`.
fn cmd_op_push(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut remote: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut since: Option<String> = None;
    let mut token: Option<String> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
            }
            "--since" => {
                since = Some(it.next().ok_or_else(|| anyhow!("--since needs an op_id"))?.clone());
            }
            "--token" => {
                token = Some(it.next().ok_or_else(|| anyhow!("--token needs a value"))?.clone());
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| anyhow!(
        "usage: lex op push <remote_url> [--branch NAME] [--since OP_ID] [--dry-run] [--store DIR] [--token TOKEN]\n\
         auth: set LEXHUB_TOKEN env var or pass --token to authenticate against lex-hub"
    ))?;
    let token = resolve_token(token);
    let store = Store::open(&root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    let local_head = store.get_branch(&branch)?.and_then(|b| b.head_op);
    let log = OpLog::open(&root)?;

    // Resolve `--since`: explicit > probe > local.
    let cutoff: Option<String> = match since {
        Some(s) => Some(s),
        None => probe_remote_head(&remote, &branch, token.as_deref()).unwrap_or(None),
    };

    let to_send: Vec<OperationRecord> = match local_head.as_ref() {
        Some(head) => {
            // ops_since walks newest-first and excludes everything
            // reachable from `cutoff`. Reverse so the batch is sent
            // oldest-first — the server's DAG-integrity check
            // requires every op's parents to either already exist
            // or appear earlier in the same batch.
            let mut ops = log.ops_since(head, cutoff.as_ref())?;
            ops.reverse();
            ops
        }
        None => Vec::new(),
    };

    if dry_run {
        let ids: Vec<&String> = to_send.iter().map(|r| &r.op_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "branch": branch,
            "since": cutoff,
            "would_send": to_send.len(),
            "op_ids": ids,
        });
        let count = to_send.len();
        let remote_text = remote.clone();
        let branch_text = branch.clone();
        acli::emit_or_text("op-push", data, fmt, move || {
            println!(
                "would push {count} ops to {remote_text} on branch `{branch_text}` (dry-run)"
            );
        });
        return Ok(());
    }

    if to_send.is_empty() {
        let data = serde_json::json!({
            "remote": remote,
            "branch": branch,
            "received": 0,
            "added": 0,
            "skipped": 0,
        });
        acli::emit_or_text("op-push", data, fmt, || {
            println!("nothing to push (branch is at or behind the remote)");
        });
        return Ok(());
    }

    // Post the batch.
    let url = format!("{}/v1/ops/batch", remote.trim_end_matches('/'));
    let body = serde_json::to_string(&to_send)
        .map_err(|e| anyhow!("serializing batch: {e}"))?;
    let resp = with_auth(ureq::post(&url), token.as_deref())
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status().as_u16();
    let resp_body: serde_json::Value = resp.into_body().read_json()
        .map_err(|e| anyhow!("decoding response: {e}"))?;
    if status == 401 {
        bail!("remote requires auth (HTTP 401) — set LEXHUB_TOKEN or pass --token");
    }
    if status >= 400 {
        bail!("server rejected batch (HTTP {status}): {resp_body}");
    }

    let received = resp_body.get("received").and_then(|v| v.as_u64()).unwrap_or(0);
    let added = resp_body.get("added").and_then(|v| v.as_u64()).unwrap_or(0);
    let skipped = resp_body.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);
    let data = serde_json::json!({
        "remote": remote,
        "branch": branch,
        "received": received,
        "added": added,
        "skipped": skipped,
    });
    let remote_text = remote.clone();
    let branch_text = branch.clone();
    acli::emit_or_text("op-push", data, fmt, move || {
        println!(
            "pushed {received} ops to {remote_text} on branch `{branch_text}`: \
             {added} added, {skipped} skipped (already present)"
        );
    });
    Ok(())
}

/// Probe the remote's head_op for `branch`. Returns Ok(Some(id))
/// when the remote knows the branch, Ok(None) when it doesn't,
/// Err(_) on transport failure. The caller treats Err as "fall
/// back to sending everything we have."
pub(crate) fn probe_remote_head(remote: &str, branch: &str, token: Option<&str>) -> Result<Option<String>> {
    let url = format!(
        "{}/v1/branches/{branch}/head",
        remote.trim_end_matches('/'),
    );
    let resp = with_auth(ureq::get(&url), token)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status == 401 {
        bail!("remote requires auth (HTTP 401) — set LEXHUB_TOKEN or pass --token");
    }
    let body: serde_json::Value = resp.into_body().read_json()
        .map_err(|e| anyhow!("decoding response: {e}"))?;
    Ok(body.get("head_op")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// `lex op pull <remote_url> [--branch NAME] [--since OP_ID]
/// [--limit N] [--dry-run] [--store DIR]` (#260).
///
/// Append-only fetch — the inverse of `lex op push`. Asks the
/// remote for ops reachable from its `branch.head_op` but not from
/// the local branch's head, validates each, and persists. On
/// fast-forward (local head is an ancestor of the new remote head)
/// the local branch's `head_op` advances; on divergent histories
/// the pull refuses with a structured envelope and the local
/// branch is unchanged.
///
/// `--since` overrides the cutoff explicitly (useful for partial
/// pulls). `--limit` chunks the response so very large gaps don't
/// require a single huge round-trip; the client re-issues until
/// the remote reports an empty response.
fn cmd_op_pull(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut remote: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut since: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut token: Option<String> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
            }
            "--since" => {
                since = Some(it.next().ok_or_else(|| anyhow!("--since needs an op_id"))?.clone());
            }
            "--limit" => {
                limit = Some(it.next().ok_or_else(|| anyhow!("--limit needs N"))?
                    .parse().map_err(|e| anyhow!("--limit: {e}"))?);
            }
            "--token" => {
                token = Some(it.next().ok_or_else(|| anyhow!("--token needs a value"))?.clone());
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| anyhow!(
        "usage: lex op pull <remote_url> [--branch NAME] [--since OP_ID] [--limit N] [--dry-run] [--store DIR] [--token TOKEN]\n\
         auth: set LEXHUB_TOKEN env var or pass --token to authenticate against lex-hub"
    ))?;
    let token = resolve_token(token);
    let store = Store::open(&root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    let local_head = store.get_branch(&branch)?.and_then(|b| b.head_op);
    let cutoff: Option<String> = since.or_else(|| local_head.clone());

    // Fetch the delta from the remote. Returns oldest-first so we
    // can apply in topological order with the existing idempotent
    // OpLog::put.
    let received = fetch_ops_since(&remote, &branch, cutoff.as_deref(), limit, token.as_deref())?;

    if dry_run {
        let ids: Vec<&String> = received.iter().map(|r| &r.op_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "branch": branch,
            "since": cutoff,
            "would_receive": received.len(),
            "op_ids": ids,
        });
        let count = received.len();
        let remote_text = remote.clone();
        let branch_text = branch.clone();
        acli::emit_or_text("op-pull", data, fmt, move || {
            println!(
                "would pull {count} ops from {remote_text} on branch `{branch_text}` (dry-run)"
            );
        });
        return Ok(());
    }

    if received.is_empty() {
        let data = serde_json::json!({
            "remote": remote,
            "branch": branch,
            "received": 0,
            "added": 0,
            "fast_forwarded_to": serde_json::Value::Null,
        });
        acli::emit_or_text("op-pull", data, fmt, || {
            println!("nothing to pull (local is at or ahead of remote)");
        });
        return Ok(());
    }

    // Validate + persist. The local op log is idempotent, so we
    // can safely re-apply ops that may already be present.
    let log = OpLog::open(&root)?;
    let mut added = 0usize;
    let mut batch_ids: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for rec in &received {
        // Content-addressing: the supplied op_id must equal the
        // canonical hash of the payload. Otherwise the remote is
        // serving forged or corrupted records.
        let expected = rec.op.op_id();
        if expected != rec.op_id {
            bail!(
                "remote returned op with mismatched op_id: supplied={}, expected={}",
                rec.op_id, expected,
            );
        }
        // DAG integrity: every parent must already be in the local
        // log OR appear earlier in this batch (which is sorted
        // oldest-first by the server).
        for parent in &rec.op.parents {
            let known = log.get(parent)?.is_some() || batch_ids.contains(parent);
            if !known {
                bail!(
                    "remote returned op {} with unreachable parent {parent}; \
                     pull aborted to preserve DAG integrity",
                    rec.op_id,
                );
            }
        }
        let was_present = log.get(&rec.op_id)?.is_some();
        log.put(rec)?;
        if !was_present {
            added += 1;
        }
        batch_ids.insert(rec.op_id.clone());
    }

    // Divergent-history detection: a clean fast-forward requires
    // that the local head, if present, is reachable from the
    // remote tip. Walk the new tip's ancestry; if local_head doesn't
    // appear, the histories diverged.
    let new_tip = received.last().expect("checked is_empty above").op_id.clone();
    let fast_forward_ok = match &local_head {
        None => true, // empty branch can absorb anything
        Some(lh) => log.walk_back(&new_tip, None)?
            .iter()
            .any(|r| &r.op_id == lh),
    };

    if !fast_forward_ok {
        let envelope = serde_json::json!({
            "error": "DivergentHistory",
            "local_head": local_head,
            "remote_head": new_tip,
            "remark": "local branch head is not an ancestor of the pulled tip; \
                      use `lex merge` to integrate the divergent histories",
        });
        // Print structured envelope; exit non-zero so scripts can
        // tell the difference from a successful no-op pull.
        let env_clone = envelope.clone();
        acli::emit_or_text("op-pull", envelope, fmt, move || {
            eprintln!("{}", serde_json::to_string_pretty(&env_clone).unwrap());
        });
        bail!("divergent histories — branch unchanged");
    }

    // Fast-forward: advance the branch head to the new tip.
    // `Store::set_branch_head_op` is `pub(crate)`, so we go through
    // the JSON file directly. (#262 will replace this with a CAS
    // path; for now we rely on the single-writer invariant.)
    fast_forward_branch_head(&root, &branch, &new_tip)
        .with_context(|| format!("advancing branch head to {new_tip}"))?;

    let data = serde_json::json!({
        "remote": remote,
        "branch": branch,
        "received": received.len(),
        "added": added,
        "fast_forwarded_to": new_tip,
    });
    let total = received.len();
    let remote_text = remote.clone();
    let branch_text = branch.clone();
    let new_tip_text = new_tip.clone();
    acli::emit_or_text("op-pull", data, fmt, move || {
        println!(
            "pulled {total} ops from {remote_text} on branch `{branch_text}`: \
             {added} new, branch advanced to {new_tip_text}"
        );
    });
    Ok(())
}

/// Fetch a delta from `<remote>/v1/ops/since`. Returns the records
/// in the order the server sent them (oldest-first by contract).
fn fetch_ops_since(
    remote: &str,
    branch: &str,
    after: Option<&str>,
    limit: Option<usize>,
    token: Option<&str>,
) -> Result<Vec<OperationRecord>> {
    let mut url = format!(
        "{}/v1/ops/since?branch={branch}",
        remote.trim_end_matches('/'),
    );
    if let Some(a) = after { url.push_str(&format!("&after={a}")); }
    if let Some(n) = limit { url.push_str(&format!("&limit={n}")); }
    let resp = with_auth(ureq::get(&url), token)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status == 401 {
        bail!("remote requires auth (HTTP 401) — set LEXHUB_TOKEN or pass --token");
    }
    if status >= 400 {
        let body = resp.into_body().read_to_string()
            .unwrap_or_else(|_| "(unreadable body)".into());
        bail!("server returned HTTP {status}: {body}");
    }
    resp.into_body().read_json::<Vec<OperationRecord>>()
        .map_err(|e| anyhow!("decoding response from {url}: {e}"))
}

/// Advance `<root>/branches/<name>.json`'s `head_op` to `new`. The
/// store's `set_branch_head_op` is `pub(crate)`, so we operate on
/// the JSON file directly. Same temp-file + rename pattern as the
/// `lex store migrate-ops` branch-head rewriter.
fn fast_forward_branch_head(
    root: &std::path::Path,
    branch: &str,
    new: &str,
) -> Result<()> {
    let path = root.join("branches").join(format!("{branch}.json"));
    if !path.exists() {
        // First-time pull on a branch the local doesn't have yet.
        // Bootstrap a minimal branch file pointing at the new head.
        std::fs::create_dir_all(path.parent().unwrap())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let value = serde_json::json!({
            "name": branch,
            "parent": serde_json::Value::Null,
            "head_op": new,
            "merges": [],
            "created_at": now,
        });
        let bytes = serde_json::to_vec_pretty(&value)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        return Ok(());
    }
    let bytes = std::fs::read(&path)?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    value["head_op"] = serde_json::Value::String(new.to_string());
    let new_bytes = serde_json::to_vec_pretty(&value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &new_bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
