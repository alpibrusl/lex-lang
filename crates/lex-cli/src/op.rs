//! `lex op show` and `lex op log`.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_store::Store;
use lex_vcs::{OpLog, OperationRecord};
use std::path::PathBuf;

use crate::sync_client::{request_json, request_json_with_header, Retry, RetryPolicy, SyncError};
use lex_api::handlers::{CAP_FILES_V1, CAP_INTENT_ORIGIN_V1};

/// Prepare an outgoing request: disable ureq's "non-2xx is an error"
/// behaviour, announce this client's capabilities, and attach
/// `Authorization: Bearer <token>` when a token is present (absent →
/// unmodified, so unauthenticated `lex serve` remotes keep working, #630).
///
/// ureq 3.x treats 4xx/5xx status codes as transport errors by default, so
/// `.send()`/`.call()` bail before the caller can read the status — which
/// left the `status == 401` / `status >= 400` branches unreachable (the 401
/// auth hint never fired; batch-rejection bodies were never surfaced).
/// Turning `http_status_as_error` off returns `Ok(resp)` for error statuses
/// so those handlers run.
///
/// #1007 PR 5: every request carries `X-Lex-Caps: files-v1` — this build
/// always understands `SetFiles` ops and their out-of-band blobs, so
/// `/v1/ops/since` never needs to 426 a pull against it (see
/// `ops_since_http::ops_since_handler`'s `files_v1` gate), and a server that
/// doesn't recognize the header simply ignores it (additive, per #1007 §4).
pub(crate) fn with_auth<B>(
    req: ureq::RequestBuilder<B>,
    token: Option<&str>,
) -> ureq::RequestBuilder<B> {
    let req = req
        .config()
        .http_status_as_error(false)
        .build()
        .header("X-Lex-Caps", CAP_FILES_V1);
    match token {
        Some(t) => req.header("Authorization", &format!("Bearer {t}")),
        None => req,
    }
}

/// Resolve the Bearer token: `--token` flag > `LEXHUB_TOKEN` env var > None.
pub(crate) fn resolve_token(flag: Option<String>) -> Option<String> {
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
    let root = root.unwrap_or_else(crate::default_store_root_pub);
    (root, rest)
}

pub fn cmd_op(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex op {{show|log|replay|push|pull|repack|gc}} [--store DIR] ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "show"   => cmd_op_show(fmt, rest),
        "log"    => cmd_op_log(fmt, rest),
        "replay" => cmd_op_replay(fmt, rest),
        "push"   => cmd_op_push(fmt, rest),
        "pull"   => cmd_op_pull(fmt, rest),
        "repack" => cmd_op_repack(fmt, rest),
        "gc"     => cmd_op_gc(fmt, rest),
        other    => bail!("unknown `lex op` subcommand: {other}"),
    }
}

/// `lex op replay <op_id> [--candidate FILE | --ollama [MODEL] |
/// --regenerate-cmd CMD] [--store DIR]` — #836 G3, replay-as-verification.
///
/// With no regenerator flag, prints the *replay request*: the recorded
/// intent (prompt / model / session), the target sig + signature, the
/// expected stage, and the parent program the change was made against.
///
/// A regenerator produces candidate Lex source, which lex parses,
/// extracts the target sig's stage from, compares to the recorded
/// stage, and records as a `Replay` attestation:
/// * `--candidate FILE` — source you already regenerated.
/// * `--ollama [MODEL]` — a local Ollama daemon (`OLLAMA_HOST`, default
///   `http://localhost:11434`); MODEL defaults to the recorded model's
///   name, else `qwen3.8:27b-mlx`.
/// * `--regenerate-cmd CMD` — any command; the request JSON is piped to
///   its stdin and Lex source read from its stdout (wire opencode, an
///   Anthropic call, anything).
fn cmd_op_replay(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut op_id: Option<String> = None;
    let mut candidate: Option<PathBuf> = None;
    let mut ollama: Option<Option<String>> = None; // Some(model?) when --ollama given
    let mut regen_cmd: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--candidate" => { candidate = args.get(i + 1).map(PathBuf::from); i += 2; }
            "--regenerate-cmd" => { regen_cmd = args.get(i + 1).cloned(); i += 2; }
            "--ollama" => {
                // Optional model argument (anything not starting with `--`).
                match args.get(i + 1) {
                    Some(m) if !m.starts_with("--") => { ollama = Some(Some(m.clone())); i += 2; }
                    _ => { ollama = Some(None); i += 1; }
                }
            }
            "--store" => { store_root = args.get(i + 1).map(PathBuf::from); i += 2; }
            other if !other.starts_with("--") && op_id.is_none() => {
                op_id = Some(other.to_string()); i += 1;
            }
            other => bail!("unexpected arg `{other}` (usage: lex op replay <op_id> \
                [--candidate FILE | --ollama [MODEL] | --regenerate-cmd CMD] [--store DIR])"),
        }
    }
    let op_id = op_id.ok_or_else(|| anyhow!("usage: lex op replay <op_id> \
        [--candidate FILE | --ollama [MODEL] | --regenerate-cmd CMD] [--store DIR]"))?;
    let regenerators = candidate.is_some() as u8 + ollama.is_some() as u8 + regen_cmd.is_some() as u8;
    if regenerators > 1 {
        bail!("choose at most one of --candidate, --ollama, --regenerate-cmd");
    }
    let root = store_root.unwrap_or_else(crate::default_store_root_pub);
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;

    // No regenerator → print the request for an external one.
    if regenerators == 0 {
        let req = store.replay_request(&op_id)?;
        let data = serde_json::to_value(&req)?;
        acli::emit_or_text("op-replay", data, fmt, move || {
            println!("replay request for op {}", req.op_id);
            println!("  target:          {}", req.target_signature.as_deref().unwrap_or(&req.target_sig));
            println!("  expected stage:  {}", req.expected_stage_id);
            println!("  model:           {}", req.model.as_deref().unwrap_or("(none recorded)"));
            match &req.prompt {
                Some(p) => println!("  prompt:          {}", p.lines().next().unwrap_or("")),
                None => println!("  prompt:          (none recorded)"),
            }
            println!("  parent program:  {} byte(s)", req.parent_program.len());
            print_skipped(&req);
            println!("\nRegenerate with --candidate FILE, --ollama [MODEL], or --regenerate-cmd CMD\nto record the Replay attestation.");
        });
        return Ok(());
    }

    // Obtain candidate Lex source from the chosen regenerator.
    let req = store.replay_request(&op_id)?;
    let (src, how): (String, String) = if let Some(path) = &candidate {
        (
            std::fs::read_to_string(path).with_context(|| format!("reading candidate {}", path.display()))?,
            format!("candidate {}", path.display()),
        )
    } else if let Some(model_opt) = &ollama {
        let model = model_opt
            .clone()
            .or_else(|| req.model.as_ref().and_then(|m| m.rsplit('/').next().map(str::to_string)))
            .unwrap_or_else(|| "qwen3.8:27b-mlx".to_string());
        (crate::replay_runner::regenerate_ollama(&req, &model)?, format!("ollama:{model}"))
    } else {
        let cmd = regen_cmd.as_deref().unwrap();
        (crate::replay_runner::regenerate_cmd(&req, cmd)?, format!("cmd `{cmd}`"))
    };

    // Parse the regenerated source and pull out the candidate for the
    // target function. Recognition is by *callable identity* (name + param
    // types + return + effects), NOT by sig_id: a fresh regeneration is
    // given the intent and type signature, not the recorded `examples {}`
    // block — and examples are folded into sig_id, so a sig_id match would
    // reject every regeneration of a function lex-code wrote with examples
    // (the common case). Exact reproduction still requires a byte-identical
    // stage (examples included); a same-callable body that isn't identical
    // falls through to the behavioral tier. A regeneration that doesn't
    // parse, or defines no matching function, is a legitimate negative
    // result — recorded, not a hard error.
    // #980: compare in DE-MANGLED space. A package publish mangles every
    // declaration (`lib_<hash>.twice`), but a dotted name cannot be written as
    // Lex source — so a regenerator necessarily emits the bare name, and
    // matching it against the mangled record made every package-published op
    // replay as a false negative. `replay_request` now reports the bare
    // name/signature, so the regeneration and the record meet in the same
    // space. The attestation still carries the real (mangled) stage id:
    // `replay_record` looks it up from the op, so provenance is unchanged.
    // #868: reconstruct skipping unloadable declarations (reported on
    // `req.skipped`) rather than failing — or, as before, silently falling
    // back to an empty program and losing the behavioral tier entirely.
    let expected_stages = store
        .demangled_program_at_op_skipping(&op_id)
        .map(|r| r.stages)
        .unwrap_or_default();
    // #868: a negative verdict under incomplete context is weaker evidence —
    // say so in the attestation's detail, not just in this process's output.
    let ctx_note = req.context_note();
    let detail = |d: String| match &ctx_note {
        Some(n) => format!("{d} ({n})"),
        None => d,
    };
    // The recorded stage id addresses the *mangled* declaration, so it won't be
    // found here; locate the target by the bare name instead, falling back to
    // the id for a head that was never mangled (a single-file publish).
    let recorded_fd = req
        .target_name
        .as_deref()
        .and_then(|want| {
            expected_stages.iter().find_map(|s| match s {
                lex_ast::Stage::FnDecl(fd) if fd.name == want => Some(fd),
                _ => None,
            })
        })
        .or_else(|| crate::behavioral::fndecl_at_stage(&expected_stages, &req.expected_stage_id));
    // What "exact reproduction" means in this space: byte-identical to the
    // recorded stage once its storage prefix is gone.
    let expected_id_demangled = recorded_fd
        .and_then(|fd| lex_ast::stage_id(&lex_ast::Stage::FnDecl(fd.clone())))
        .unwrap_or_else(|| req.expected_stage_id.clone());
    let outcome = match lex_syntax::parse_source(&src) {
        Err(e) => store.replay_record_miss(&op_id, &detail(format!("regenerated source did not parse: {e:?}")))?,
        Ok(prog) => {
            let stages = lex_ast::canonicalize_program(&prog);
            let cand = match recorded_fd {
                Some(rec) => stages.into_iter().find(|st| {
                    matches!(st, lex_ast::Stage::FnDecl(fd) if crate::behavioral::same_callable(rec, fd))
                }),
                // Couldn't load the recorded fn (e.g. reconstruction gap) —
                // fall back to the exact sig_id match.
                None => stages
                    .into_iter()
                    .find(|st| lex_ast::sig_id(st).as_deref() == Some(req.target_sig.as_str())),
            };
            match cand {
                Some(cand) => {
                    let produced = lex_ast::stage_id(&cand);
                    let exact = produced.as_deref() == Some(expected_id_demangled.as_str());
                    if exact {
                        store.replay_record(&op_id, produced, true, None, None)?
                    } else if let Some(pid) = produced {
                        // Same function, not byte-identical: the same body
                        // written differently (if vs match, `>` vs `>=`), or
                        // the same body with a different examples block. Try
                        // the behavioral tier — recorded distinctly
                        // (behavioral_samples), never conflated with exact.
                        let behavioral = crate::behavioral::behavioral_equiv(
                            &expected_stages,
                            &cand,
                            &expected_id_demangled,
                        );
                        match behavioral {
                            Some(n) => store.replay_record(&op_id, Some(pid), true, Some(n), None)?,
                            None => store.replay_record(&op_id, Some(pid), false, None,
                                Some(detail("regeneration did not reproduce the recorded stage".into())))?,
                        }
                    } else {
                        store.replay_record(&op_id, None, false, None,
                            Some(detail("regenerated candidate had no hashable stage".into())))?
                    }
                }
                None => store.replay_record_miss(&op_id, &detail(format!(
                    "regenerated source did not define the target function {}",
                    req.target_name.as_deref().unwrap_or(&req.target_sig))))?,
            }
        }
    }
    .with_context_of(&req);
    let data = serde_json::to_value(&outcome)?;
    acli::emit_or_text("op-replay", data, fmt, move || {
        if outcome.reproduced {
            match outcome.behavioral_samples {
                Some(n) => println!(
                    "reproduced behaviorally ({how}): op {} — the candidate differs from the \
                     recorded stage {} but returns the same value over {n} sampled input(s)",
                    outcome.op_id, outcome.expected_stage_id),
                None => println!("reproduced ({how}): op {} regenerates to the recorded stage {}",
                    outcome.op_id, outcome.expected_stage_id),
            }
        } else {
            println!("NOT reproduced ({how}): op {} expected {} but the candidate produced {}",
                outcome.op_id, outcome.expected_stage_id,
                outcome.produced_stage_id.as_deref().unwrap_or("(different sig / nothing)"));
        }
        println!("  Replay attestation: {}", outcome.attestation_id);
        if outcome.context_incomplete {
            println!("  note: replayed against an incomplete parent program");
            print_skipped(&req);
        }
    });
    Ok(())
}

/// List the parent-program declarations replay had to skip (#868).
fn print_skipped(req: &lex_store::ReplayRequest) {
    if req.skipped.is_empty() {
        return;
    }
    println!("  skipped:         {} unloadable declaration(s) left out of the parent program", req.skipped.len());
    for sk in &req.skipped {
        println!(
            "    - {} (stage {}){}: {}",
            sk.name.as_deref().unwrap_or(&sk.sig_id),
            sk.stage_id,
            if sk.called_by_target { " [called by the target]" } else { "" },
            sk.reason,
        );
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
///
/// `lex op gc --blobs {--dry-run|--confirm} [--grace-hours N] [--store DIR]`
/// (#1007 §3 / PR 7): a separate mode, mark-and-sweep over the blob space
/// instead of the op log — live = every blob in a retained `SetFiles`
/// op's manifest closure, plus everything bound under `blobrefs/**`
/// (locks, loom artifacts). `--retain` doesn't apply here (blob
/// liveness follows op retention, it isn't independently predicated);
/// `--grace-hours` (default 24) is the minimum age before an
/// unreferenced blob is swept, protecting a blob an in-flight push
/// uploaded moments ago but whose op hasn't landed yet (push order is
/// blobs → ops → head, per #1007 §4).
fn cmd_op_gc(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut dry_run = false;
    let mut confirm = false;
    let mut blobs = false;
    let mut grace_hours: u64 = 24;
    let mut cli_retain: Vec<lex_vcs::Predicate> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => dry_run = true,
            "--confirm" => confirm = true,
            "--blobs" => blobs = true,
            "--grace-hours" => {
                let raw = it.next()
                    .ok_or_else(|| anyhow!("--grace-hours needs N"))?;
                grace_hours = raw.parse()
                    .map_err(|e| anyhow!("--grace-hours: {e}"))?;
            }
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
                [--dry-run|--confirm] [--retain JSON]... [--store DIR], \
                or lex op gc --blobs [--dry-run|--confirm] [--grace-hours N] [--store DIR])"),
        }
    }
    if !dry_run && !confirm {
        bail!("`lex op gc` requires either --dry-run or --confirm");
    }
    if dry_run && confirm {
        bail!("--dry-run and --confirm are mutually exclusive");
    }
    let store = Store::open(&root)?;

    if blobs {
        let grace = std::time::Duration::from_secs(grace_hours.saturating_mul(3600));
        let plan = store.plan_blob_gc(grace)?;
        let removed = if confirm { store.apply_blob_gc(&plan)? } else { 0 };
        let data = serde_json::json!({
            "store": root.display().to_string(),
            "dry_run": dry_run,
            "grace_hours": grace_hours,
            "blobs_to_delete": &plan.to_delete,
            "blobs_within_grace": plan.skipped_within_grace.len(),
            "blobs_live_count": plan.live.len(),
            "removed": removed,
        });
        acli::emit_or_text("op", data, fmt, || {
            let n = plan.to_delete.len();
            if dry_run {
                println!("plan: would delete {n} blob(s); {} live, {} within grace",
                    plan.live.len(), plan.skipped_within_grace.len());
            } else if removed == 0 {
                println!("nothing to do (no unreferenced blobs past the grace period)");
            } else {
                println!("removed {removed} blob(s); {} live", plan.live.len());
            }
        });
        return Ok(());
    }

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
    // #1007: a files snapshot is recorded history but not a program change;
    // mark it so a reader never mistakes it for a semantic edit.
    let files_mark = if r.op.kind.is_semantic() { "" } else { " [files]" };
    println!("kind:    {kind_label}{files_mark}");
    if let lex_vcs::OperationKind::SetFiles { manifest } = &r.op.kind {
        println!("files:   {manifest}");
    }
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
        // #1031: a re-lock (`lex pkg lock` + re-publish) of a head that was
        // already pushed produces zero new ops — the head itself doesn't
        // move — but the *committed lock* at that head can still have
        // changed underneath it (a dependency re-resolved to a new pin).
        // Sync it even though there's nothing else to push, or the remote's
        // write-time gate keeps resolving against the stale pin it already
        // had, or (if it never had one) never learns of it at all — worse
        // than the known "rewrites an already-pushed head's lock" gap
        // (#1007), because this path used to skip the sync attempt
        // entirely.
        if let Some(head) = local_head.as_ref() {
            if let Some(lock) = store.committed_lock(head)? {
                post_json(
                    &remote,
                    "/v1/locks/batch",
                    &serde_json::json!([{ "head_op": head, "lock": lock }]),
                    token.as_deref(),
                )?;
            }
        }
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

    // #1007 PR 5: a `SetFiles` op's blob contents travel out of band
    // (`/v1/blobs/*`), which only a hub advertising `files-v1` understands.
    // An older hub has no such routes at all; a hub that runs the #1007
    // `ops/batch` gate (`check_set_files`) but was never told about our
    // caps would only discover the problem after we'd already uploaded the
    // stage/intent content for nothing and posted the batch, refusing it
    // with `MissingBlobs` (its own blob routes never having been asked).
    // Check the remote's advertised capabilities BEFORE uploading anything
    // — refuse fast and cleanly instead.
    if to_send.iter().any(|r| matches!(r.op.kind, lex_vcs::OperationKind::SetFiles { .. })) {
        let caps = remote_health_caps(&remote, token.as_deref()).map_err(|e| {
            anyhow!(
                "this push includes a files snapshot (SetFiles, #1007), but checking the \
                 remote's capabilities first (GET {remote}/v1/health) failed: {e}"
            )
        })?;
        if !caps.iter().any(|c| c.eq_ignore_ascii_case(CAP_FILES_V1)) {
            bail!(
                "cannot push: this push includes a files snapshot (SetFiles, #1007) but the \
                 remote at {remote} does not advertise `{CAP_FILES_V1}` support (GET /v1/health \
                 caps: {caps:?}) — upgrade the hub to a #1007-capable lex-hub, or publish/push a \
                 history without files (`lex publish --no-files`). Nothing was uploaded."
            );
        }
    }

    // #892 PR 2: an intent carrying `origin` (external-VCS provenance)
    // only survives a hub that stores the field. An older hub would
    // deserialize the intent, silently drop `origin` and re-store bytes that
    // no longer match the intent's id. Check the remote's advertised
    // capabilities BEFORE uploading anything — refuse fast and cleanly.
    // Pushes with no origin-bearing intent never even query `/v1/health`.
    let origin_intents = origin_bearing_intents(&store, &to_send)?;
    if !origin_intents.is_empty() {
        let caps = remote_health_caps(&remote, token.as_deref()).map_err(|e| {
            anyhow!(
                "this push includes {} intent(s) with git-import provenance (`origin`, #892), \
                 but checking the remote's capabilities first (GET {remote}/v1/health) failed: {e}",
                origin_intents.len()
            )
        })?;
        if !caps.iter().any(|c| c.eq_ignore_ascii_case(CAP_INTENT_ORIGIN_V1)) {
            bail!(
                "cannot push: this push includes {} intent(s) carrying `origin` provenance \
                 (#892, e.g. {}) but the remote at {remote} does not advertise \
                 `{CAP_INTENT_ORIGIN_V1}` support (GET /v1/health caps: {caps:?}) — an older \
                 hub would silently drop the provenance. Upgrade the hub to an \
                 #892-capable lex-hub. Nothing was uploaded.",
                origin_intents.len(),
                origin_intents[0],
            );
        }
    }

    // Content first: push the stage + intent blobs these ops reference, so
    // a peer that pulls the op records always has the objects they point at
    // (without this, a pulled op-log renders as `unknown stage_id`).
    push_objects(&remote, &to_send, &store, token.as_deref())?;

    // Completeness safety net: guarantee the remote can render every op
    // reachable from the head we're advancing to — not just the delta above.
    // Closes the incremental/multi-store stranding that caused the
    // multi-module archive-500 (see `reconcile_head_stages`).
    if let Some(head) = local_head.as_ref() {
        reconcile_head_stages(&remote, head, &store, token.as_deref())?;
    }

    // #1007 PR 5: push order is stages/intents → blobs → locks/issues → ops
    // → head (design §4). Blobs must land before `/v1/ops/batch` sees the
    // `SetFiles` records that name them, or the server's `check_set_files`
    // gate refuses with `MissingBlobs`.
    let blobs_pushed = push_blobs(&remote, &to_send, &store, token.as_deref())?;

    // #930 P2b-1: send the committed lex.lock for the head we're advancing to,
    // so the remote's write-time gate can resolve this head's pinned
    // dependencies. A dependency-free head has no committed lock — skip it.
    if let Some(head) = local_head.as_ref() {
        if let Some(lock) = store.committed_lock(head)? {
            post_json(
                &remote,
                "/v1/locks/batch",
                &serde_json::json!([{ "head_op": head, "lock": lock }]),
                token.as_deref(),
            )?;
        }
    }

    // #949 phase 1: typed issues travel with the package. An open issue isn't
    // reachable from any op, so send the whole local log — content-addressed
    // and idempotent server-side, a re-push converges.
    {
        let ilog = lex_vcs::IssueLog::open(store.root())?;
        let issues: Vec<lex_vcs::Issue> = ilog
            .list_ids()?
            .iter()
            .filter_map(|id| ilog.get(id).ok().flatten())
            .collect();
        if !issues.is_empty() {
            post_json(&remote, "/v1/issues/batch", &serde_json::to_value(&issues)?, token.as_deref())?;
        }
    }

    // Post the batch.
    // Retry-safe: the server skips any record whose content-addressed
    // `op_id` it already holds (`ops_batch_handler`), so a re-send after a
    // lost response converges to `added: 0` instead of duplicating.
    let body = serde_json::to_string(&to_send)
        .map_err(|e| anyhow!("serializing batch: {e}"))?;
    let resp_body: serde_json::Value = request_json(
        &remote,
        "/v1/ops/batch",
        Some(&body),
        token.as_deref(),
        Retry::Idempotent,
        &RetryPolicy::from_env(),
    )?;

    let received = resp_body.get("received").and_then(|v| v.as_u64()).unwrap_or(0);
    let added = resp_body.get("added").and_then(|v| v.as_u64()).unwrap_or(0);
    let skipped = resp_body.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);

    // The ref half: the ops batch above transferred the object DAG; now
    // advance the remote branch head to our local head. Fast-forward only
    // (server-enforced) — a non-fast-forward is a real error the user must
    // see (their branch diverged), not a silent no-op. Without this step a
    // push lands the objects but nothing points at them, so the remote is
    // unpullable — the bug this fixes.
    let mut advance = String::from("unchanged");
    if let Some(head) = local_head.as_ref() {
        let head_body = serde_json::json!({ "head_op": head }).to_string();
        // Not retried: a ref update is only conditionally idempotent, and a
        // retry racing a concurrent push could turn a success into a 409.
        let hbody: serde_json::Value = match request_json(
            &remote,
            &format!("/v1/branches/{branch}/head"),
            Some(&head_body),
            token.as_deref(),
            Retry::Once,
            &RetryPolicy::from_env(),
        ) {
            Ok(v) => v,
            Err(SyncError::Status { status: 409, body, .. }) => bail!(
                "non-fast-forward: the remote branch `{branch}` has diverged from your local \
                 head. The ops were uploaded, but the branch was not advanced. Pull and \
                 reconcile first. ({body})"
            ),
            Err(e) => {
                // #992: the hub's 422 `UnsatisfiablePair` names the pair and
                // the fix; print that rather than the raw body.
                if let SyncError::Status { status, body, .. } = &e {
                    let parsed = serde_json::from_str::<serde_json::Value>(body).ok();
                    if let Some(msg) =
                        parsed.and_then(|v| unsatisfiable_pair_message(*status, &v))
                    {
                        bail!("{msg}");
                    }
                }
                return Err(e.into());
            }
        };
        advance = hbody.get("advance").and_then(|a| a.as_str()).unwrap_or("advanced").to_string();
    }

    let data = serde_json::json!({
        "remote": remote,
        "branch": branch,
        "received": received,
        "added": added,
        "skipped": skipped,
        "blobs_pushed": blobs_pushed,
        "head_advance": advance,
    });
    let remote_text = remote.clone();
    let branch_text = branch.clone();
    let advance_text = advance.clone();
    acli::emit_or_text("op-push", data, fmt, move || {
        println!(
            "pushed {received} ops to {remote_text} on branch `{branch_text}`: \
             {added} added, {skipped} skipped (already present), {blobs_pushed} blob(s) \
             uploaded; head {advance_text}"
        );
    });
    Ok(())
}

/// Probe the remote's head_op for `branch`. Returns Ok(Some(id))
/// when the remote knows the branch, Ok(None) when it doesn't,
/// Err(_) on transport failure. The caller treats Err as "fall
/// back to sending everything we have."
pub(crate) fn probe_remote_head(remote: &str, branch: &str, token: Option<&str>) -> Result<Option<String>> {
    let body: serde_json::Value = request_json(
        remote,
        &format!("/v1/branches/{branch}/head"),
        None,
        token,
        Retry::Idempotent,
        &RetryPolicy::from_env(),
    )?;
    Ok(body.get("head_op")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// POST a JSON body to `<remote><path>`, returning the parsed response.
/// Shared by the object-sync calls. Every caller is retry-safe: the `fetch` /
/// `missing` endpoints are read-only, and the `batch` endpoints (stages,
/// intents, issues, locks) store content-addressed and skip what they already
/// hold, so a transient failure (#971) is retried with backoff.
fn post_json(
    remote: &str,
    path: &str,
    body: &serde_json::Value,
    token: Option<&str>,
) -> Result<serde_json::Value> {
    Ok(request_json(
        remote,
        path,
        Some(&body.to_string()),
        token,
        Retry::Idempotent,
        &RetryPolicy::from_env(),
    )?)
}

/// GET `<remote><path>`, returning the parsed JSON response. The read half's
/// twin of [`post_json`], for the attestation-sync fetches.
fn get_json(remote: &str, path: &str, token: Option<&str>) -> Result<serde_json::Value> {
    Ok(request_json(remote, path, None, token, Retry::Idempotent, &RetryPolicy::from_env())?)
}

/// Stage/intent ids requested per `fetch` call. Stage blobs are ASTs and can
/// be large, so bound how many come back in one response; `pull_objects`
/// chunks its id lists by this.
const FETCH_CHUNK: usize = 256;

/// The content half of `op push`: send the stage (code) and intent blobs the
/// given ops reference. Content-addressed and idempotent server-side, so a
/// re-push converges; sending it before the op records means a peer that
/// pulls always has the objects the records point at.
fn push_objects(
    remote: &str,
    ops: &[OperationRecord],
    store: &Store,
    token: Option<&str>,
) -> Result<()> {
    use std::collections::BTreeSet;
    let mut stage_ids: BTreeSet<String> = BTreeSet::new();
    for rec in ops {
        for sid in rec.produces.stage_ids() {
            stage_ids.insert(sid);
        }
    }
    let stages: Vec<lex_ast::Stage> =
        stage_ids.iter().filter_map(|id| store.get_ast(id).ok()).collect();
    if !stages.is_empty() {
        post_json(remote, "/v1/stages/batch", &serde_json::to_value(&stages)?, token)?;
    }

    let intent_log = lex_vcs::IntentLog::open(store.root())?;
    let mut intent_ids: BTreeSet<String> = BTreeSet::new();
    for rec in ops {
        if let Some(id) = &rec.op.intent_id {
            intent_ids.insert(id.clone());
        }
    }
    let intents: Vec<lex_vcs::Intent> =
        intent_ids.iter().filter_map(|id| intent_log.get(id).ok().flatten()).collect();
    if !intents.is_empty() {
        post_json(remote, "/v1/intents/batch", &serde_json::to_value(&intents)?, token)?;
    }
    Ok(())
}

/// Ids of the intents referenced by `ops` that carry an `origin` (#892).
/// Sorted, de-duplicated (an intent is shared by many ops).
fn origin_bearing_intents(store: &Store, ops: &[OperationRecord]) -> Result<Vec<String>> {
    use std::collections::BTreeSet;
    let intent_log = lex_vcs::IntentLog::open(store.root())?;
    let mut ids: BTreeSet<&String> = BTreeSet::new();
    for rec in ops {
        if let Some(id) = &rec.op.intent_id {
            ids.insert(id);
        }
    }
    let mut out = Vec::new();
    for id in ids {
        if let Some(intent) = intent_log.get(id).ok().flatten() {
            if intent.origin.is_some() {
                out.push(id.clone());
            }
        }
    }
    Ok(out)
}

/// The `caps` array of `GET <remote>/v1/health` (#1007 §4), e.g.
/// `["files-v1", "intent-origin-v1"]`. An old hub that predates capability advertisement
/// answers `/v1/health` without a `caps` field (or doesn't have the route
/// at all, which surfaces as a transport/status error to the caller) —
/// either way this reads as "no capabilities", which is the conservative
/// answer `cmd_op_push`'s files-v1 and intent-origin-v1 gates need.
fn remote_health_caps(remote: &str, token: Option<&str>) -> Result<Vec<String>> {
    let body: serde_json::Value = get_json(remote, "/v1/health", token)?;
    Ok(body
        .get("caps")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// Hex SHA-256 of `bytes` — the same content address blobs are keyed by
/// (`Store::put_blob_bytes`, `lex-api`'s `sha256_hex`). Used client-side to
/// re-verify a blob's claimed id before trusting or storing it, both when
/// pushing (nothing here actually re-hashes on push — the store already
/// computed the id when the blob was first written) and, critically, when
/// pulling (#1007 PR 5 item 2): a blob the remote serves is only ever
/// stored once *this* hash matches what was asked for.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Request-size budget for one `/v1/blobs/{batch,fetch}` call, in *decoded*
/// bytes (§4: "chunked by manifest `size`"). Kept well under both the
/// server's 16 MiB body cap and the default 8 MiB single-blob limit even
/// after base64 inflates the wire size by ~4/3, so a handful of
/// near-the-limit files don't have to share one oversized request.
const BLOB_CHUNK_BYTES: u64 = 8 * 1024 * 1024;

/// The blob half of `op push` (#1007 PR 5): upload the content a `SetFiles`
/// op in `ops` needs — its manifest blob plus every blob the manifest's
/// entries name — that the remote does not already hold. Returns the
/// number of blobs actually uploaded (for `op push`'s summary and for
/// tests asserting an incremental push re-sends only what changed).
///
/// Scope is deliberately just `ops`' own `SetFiles` closure, not the whole
/// head's (unlike `reconcile_head_stages`'s stage reconciliation): a
/// `SetFiles` already on the remote from an earlier push was already
/// validated (and its blobs already accepted) at that time, so there is
/// nothing new to reconcile for it here.
fn push_blobs(remote: &str, ops: &[OperationRecord], store: &Store, token: Option<&str>) -> Result<usize> {
    use base64::Engine as _;
    use std::collections::BTreeSet;

    let mut ids: BTreeSet<String> = BTreeSet::new();
    for rec in ops {
        if let lex_vcs::OperationKind::SetFiles { manifest } = &rec.op.kind {
            ids.insert(manifest.clone());
            let m = store
                .get_manifest(manifest)
                .map_err(|e| anyhow!("reading files manifest {manifest} to push: {e}"))?;
            for entry in m.entries.values() {
                ids.insert(entry.blob.clone());
            }
        }
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = ids.into_iter().collect();

    // Ask the remote what it's missing rather than uploading blindly — an
    // unchanged file republished alongside a real edit (or the same
    // manifest pushed twice) costs one small request, not a re-upload.
    let resp = post_json(remote, "/v1/blobs/missing", &serde_json::json!({ "ids": ids }), token)?;
    let missing: Vec<String> = resp
        .get("missing")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if missing.is_empty() {
        return Ok(0);
    }

    let mut chunk: Vec<serde_json::Value> = Vec::new();
    let mut chunk_bytes: u64 = 0;
    let mut uploaded = 0usize;
    for id in &missing {
        let bytes = store
            .get_blob_bytes(id)
            .map_err(|e| anyhow!("reading blob {id} to push (named by a manifest we're pushing): {e}"))?;
        let len = bytes.len() as u64;
        if !chunk.is_empty() && chunk_bytes.saturating_add(len) > BLOB_CHUNK_BYTES {
            post_json(remote, "/v1/blobs/batch", &serde_json::Value::Array(std::mem::take(&mut chunk)), token)?;
            chunk_bytes = 0;
        }
        chunk.push(serde_json::json!({
            "id": id,
            "data_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }));
        chunk_bytes += len;
        uploaded += 1;
    }
    if !chunk.is_empty() {
        post_json(remote, "/v1/blobs/batch", &serde_json::Value::Array(chunk), token)?;
    }
    Ok(uploaded)
}

/// The operator-facing message for a hub's 422 `UnsatisfiablePair` refusal
/// (#992): the pushed head binds a sig to a stage filed under another sig, so
/// the remote would never be able to render it. Names the pair and prints the
/// hub's hint (republish from source, which retires the stranded entry via
/// #995). `None` for any other response.
pub(crate) fn unsatisfiable_pair_message(status: u16, body: &serde_json::Value) -> Option<String> {
    if status != 422 || body.get("error").and_then(|e| e.as_str()) != Some("UnsatisfiablePair") {
        return None;
    }
    let d = body.get("detail")?;
    let field = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
    Some(format!(
        "the remote refused this head: sig {} is bound to stage {}, but that stage is filed \
         under sig {}, so no store can hold the pair (#992). The branch was not advanced.\n\
         hint: {}",
        field("sig_id"),
        field("stage_id"),
        field("filed_under"),
        d.get("hint")
            .and_then(|v| v.as_str())
            .unwrap_or("republish from source to retire the stranded entry (#995)"),
    ))
}

/// Guarantee the remote holds every stage blob needed to render ANY op
/// reachable from `head` — not just the incremental `to_send` delta.
///
/// [`push_objects`] pushes only the stages the delta ops produce. That is
/// unsound across repeated/multi-store pushes: an op already on the remote
/// (below the push cutoff) can reference a stage that a *prior* push never
/// delivered — most visibly a `Replace`'s superseded `from` stage, which a
/// later branch tip no longer names but a pinned **release op** still renders
/// through. The result is a remote that serves the op-log but 500s on
/// `unknown stage_id` when rendering an older release (the multi-module
/// archive-500 that stranded lex-official's libraries).
///
/// This walks the full closure of `head`, asks the remote which of those
/// stages it is missing (`/v1/stages/missing`, a cheap id-only check), and
/// pushes exactly those. A stage the local store cannot produce but the remote
/// needs is a hard error (surfacing the integrity gap) rather than a silent
/// drop. On an older hub without the endpoint, it falls back to pushing the
/// whole closure (idempotent, heavier once) so correctness never depends on
/// the reconciler being present remotely.
fn reconcile_head_stages(
    remote: &str,
    head: &str,
    store: &Store,
    token: Option<&str>,
) -> Result<()> {
    use std::collections::BTreeSet;
    let log = lex_vcs::OpLog::open(store.root())?;
    // #986: reconcile (sig, stage) PAIRS, not bare stage ids. A StageId does
    // not encode the name (#826), so two sigs can share one id while holding
    // different ASTs; asking "do you have this id?" can answer yes while the
    // variant this head names is missing — which silently defeated the
    // reconciliation this function exists to perform.
    let mut closure: BTreeSet<(String, String)> = BTreeSet::new();
    for rec in log.walk_forward(&head.to_string(), None)? {
        for pair in rec.produces.stage_pairs() {
            closure.insert(pair);
        }
    }
    if closure.is_empty() {
        return Ok(());
    }
    let all_pairs: Vec<(String, String)> = closure.into_iter().collect();

    // Ask the remote which pairs it lacks. On any failure (an older hub that
    // does not understand the pair shape, say) fall back to reconciling the
    // whole closure — correctness over the round-trip saving.
    let wire: Vec<serde_json::Value> =
        all_pairs.iter().map(|(sig, st)| serde_json::json!([sig, st])).collect();
    let missing: Vec<(String, String)> = match post_json(
        remote,
        "/v1/stages/missing",
        &serde_json::json!({ "pairs": wire }),
        token,
    ) {
        Ok(v) => v
            .get("missing")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let q = p.as_array()?;
                        Some((
                            q.first()?.as_str()?.to_string(),
                            q.get(1)?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_else(|| all_pairs.clone()),
        Err(_) => all_pairs.clone(),
    };
    if missing.is_empty() {
        return Ok(());
    }

    // What the *head* names, as opposed to what history merely mentions. Only
    // the former must be present: those are the pairs a render resolves, so a
    // remote lacking one serves `500 unknown stage_id`.
    let head_pairs: BTreeSet<(String, String)> =
        lex_store::render::package_head_at_op(store, head)
            .map(|h| h.map.into_iter().collect())
            .unwrap_or_default();

    // Read each missing stage through the sig the head names it by, so the
    // right variant is sent even where several share a stage id.
    let mut stages: Vec<lex_ast::Stage> = Vec::with_capacity(missing.len());
    for (ast, (sig, stage)) in store.get_asts_for_sigs_bulk(&missing).into_iter().zip(&missing) {
        let ast = match ast {
            Ok(a) => a,
            // A pair the head names but the store cannot produce is a genuine
            // integrity gap — surface it loudly rather than ship a remote that
            // will 500 on `unknown stage_id`.
            Err(e) if head_pairs.contains(&(sig.clone(), stage.clone())) => {
                return Err(anyhow!(
                    "local store is missing stage {stage} for sig {sig}, \
                     which the head requires: {e}"
                ));
            }
            // A purely historical pair is best-effort (#992). Some of them
            // *cannot* exist: a pre-#992 `ChangeEffectSig` bound the old sig to
            // the new stage, and the AST there hashes to a different sig, so no
            // store has ever held that pair. Demanding it made every push of
            // such a package fail forever — including the push of the very
            // repair that retires the entry. It is not needed to render the
            // head, only to replay a point in history that was already
            // unrenderable when it was written.
            Err(_) => continue,
        };
        stages.push(ast);
    }
    post_json(remote, "/v1/stages/batch", &serde_json::to_value(&stages)?, token)?;
    Ok(())
}

/// The content half of `op pull`: fetch + store the stage and intent blobs
/// the pulled ops reference but the local store is missing, so `export-git`
/// and replay work on a pulled op-log.
///
/// Attestation sync (#916) is deliberately NOT part of this function — see
/// [`sync_attestations`]'s doc for why (#1030). This function only ever
/// touches the op/stage/intent DAG, which is why it's still safe for its
/// caller to treat its success as "safe to fast-forward the branch head".
fn pull_objects(
    remote: &str,
    ops: &[OperationRecord],
    store: &Store,
    token: Option<&str>,
) -> Result<(usize, usize)> {
    use std::collections::BTreeSet;
    // #986: decide what to fetch per `(sig, stage)` pair, not per stage id. A
    // StageId does not encode the name (#826), so holding that id under *some*
    // other sig does not mean the variant these ops name is present — and an
    // id-only check therefore skips fetching it, leaving a head that cannot be
    // rendered. This is where the gap originates; the push-side reconciler
    // cannot recover a variant the pull never brought down.
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for rec in ops {
        for pair in rec.produces.stage_pairs() {
            pairs.insert(pair);
        }
    }
    let pairs: Vec<(String, String)> = pairs.into_iter().collect();
    let mut want_stages: BTreeSet<String> = BTreeSet::new();
    for (have, (_sig, stage)) in store.get_asts_for_sigs_bulk(&pairs).into_iter().zip(&pairs) {
        if have.is_err() {
            want_stages.insert(stage.clone());
        }
    }
    let mut stages_added = 0usize;
    let want_stages: Vec<String> = want_stages.into_iter().collect();
    for chunk in want_stages.chunks(FETCH_CHUNK) {
        let v = post_json(remote, "/v1/stages/fetch", &serde_json::json!({ "ids": chunk }), token)?;
        if let Some(arr) = v.get("stages").and_then(|s| s.as_array()) {
            for sv in arr {
                let stage: lex_ast::Stage = serde_json::from_value(sv.clone())?;
                store.publish(&stage)?;
                stages_added += 1;
            }
        }
    }

    let intent_log = lex_vcs::IntentLog::open(store.root())?;
    let mut want_intents: BTreeSet<String> = BTreeSet::new();
    for rec in ops {
        if let Some(id) = &rec.op.intent_id {
            if intent_log.get(id)?.is_none() {
                want_intents.insert(id.clone());
            }
        }
    }
    let mut intents_added = 0usize;
    let want_intents: Vec<String> = want_intents.into_iter().collect();
    for chunk in want_intents.chunks(FETCH_CHUNK) {
        let v = post_json(remote, "/v1/intents/fetch", &serde_json::json!({ "ids": chunk }), token)?;
        if let Some(arr) = v.get("intents").and_then(|i| i.as_array()) {
            for iv in arr {
                let intent: lex_vcs::Intent = serde_json::from_value(iv.clone())?;
                intent_log.put(&intent)?;
                intents_added += 1;
            }
        }
    }

    Ok((stages_added, intents_added))
}

/// The blob half of `op pull` (#1007 PR 5): fetch + store the blobs
/// referenced by every `SetFiles` op in the pulled delta, plus the manifest
/// in force at `head_after` (covering a manifest this pull's ops merely
/// *inherit* through a merge rather than themselves set — the delta may
/// contain no `SetFiles` at all while the head it produces still names one
/// set by an earlier, already-local op). Returns the number of blobs
/// fetched. Every blob is re-hashed against its claimed id on receipt
/// (§4); a mismatch fails the whole pull rather than silently keeping the
/// blobs that did check out, so a local store can never end up holding a
/// blob filed under someone else's hash.
///
/// Called before the branch head is advanced (`cmd_op_pull`), so a failure
/// here leaves the branch where it was — the ops themselves may already be
/// in the local log (harmless: they're content-addressed and re-pullable),
/// but nothing points at the corrupt/incomplete manifest as a head.
fn pull_blobs(
    remote: &str,
    ops: &[OperationRecord],
    head_after: &str,
    store: &Store,
    token: Option<&str>,
) -> Result<usize> {
    use std::collections::BTreeSet;

    let mut manifest_ids: BTreeSet<String> = BTreeSet::new();
    for rec in ops {
        if let lex_vcs::OperationKind::SetFiles { manifest } = &rec.op.kind {
            manifest_ids.insert(manifest.clone());
        }
    }
    if let Ok(lex_store::ManifestAt::Set { manifest }) = store.manifest_at(head_after) {
        manifest_ids.insert(manifest);
    }
    if manifest_ids.is_empty() {
        return Ok(0);
    }

    let mut fetched = 0usize;

    // Manifests are small canonical JSON — one id-only fetch for all of them.
    let want_manifests: Vec<String> =
        manifest_ids.iter().filter(|id| !store.has_blob(id)).cloned().collect();
    fetched += fetch_and_verify_blobs(remote, &want_manifests, store, token)?;

    // Now that every manifest referenced is local, read them to discover
    // their entry blobs, and fetch those too — chunked by the manifest's
    // declared size (§4), not by count, so a handful of large files don't
    // share one oversized request while many small ones batch together.
    let mut want_entries: Vec<(String, u64)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for mid in &manifest_ids {
        let m = store
            .get_manifest(mid)
            .map_err(|e| anyhow!("reading pulled files manifest {mid}: {e}"))?;
        for entry in m.entries.values() {
            if seen.insert(entry.blob.clone()) && !store.has_blob(&entry.blob) {
                want_entries.push((entry.blob.clone(), entry.size));
            }
        }
    }
    let mut chunk: Vec<String> = Vec::new();
    let mut chunk_bytes: u64 = 0;
    for (id, size) in want_entries {
        if !chunk.is_empty() && chunk_bytes.saturating_add(size) > BLOB_CHUNK_BYTES {
            fetched += fetch_and_verify_blobs(remote, &std::mem::take(&mut chunk), store, token)?;
            chunk_bytes = 0;
        }
        chunk_bytes += size;
        chunk.push(id);
    }
    if !chunk.is_empty() {
        fetched += fetch_and_verify_blobs(remote, &chunk, store, token)?;
    }
    Ok(fetched)
}

/// Fetch `ids` via `POST /v1/blobs/fetch` and store each one — but only
/// after re-hashing its bytes and confirming they match the id we asked
/// for (#1007 PR 5 item 2). This is a genuine integrity check, not a
/// decode-and-trust: a hub could otherwise serve corrupted or substituted
/// bytes under a blob's name and a client would silently persist them. A
/// mismatch, or the remote simply not returning one of the requested ids,
/// fails the whole call — there is no partial-success return, so the
/// caller can treat `Ok` as "every id in `ids` is now a verified local
/// blob."
fn fetch_and_verify_blobs(remote: &str, ids: &[String], store: &Store, token: Option<&str>) -> Result<usize> {
    use base64::Engine as _;
    use std::collections::BTreeSet;

    if ids.is_empty() {
        return Ok(0);
    }
    let resp = post_json(remote, "/v1/blobs/fetch", &serde_json::json!({ "ids": ids }), token)?;
    let arr = resp.get("blobs").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut got: BTreeSet<String> = BTreeSet::new();
    for wb in &arr {
        let id = wb
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("/v1/blobs/fetch response entry missing `id`"))?;
        let data_b64 = wb
            .get("data_b64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("/v1/blobs/fetch response for `{id}` missing `data_b64`"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| anyhow!("blob `{id}`: remote sent invalid base64: {e}"))?;
        let actual = sha256_hex(&bytes);
        if actual != id {
            bail!(
                "blob integrity check failed: the remote served {} byte(s) for blob `{id}` that \
                 hash to `{actual}` instead — refusing to store it. Pull aborted before the \
                 branch head advanced (#1007 requires every blob to be re-hashed on receipt).",
                bytes.len(),
            );
        }
        store.put_blob_bytes(&bytes).map_err(|e| anyhow!("storing blob {id}: {e}"))?;
        got.insert(id.to_string());
    }
    let missing: Vec<&String> = ids.iter().filter(|id| !got.contains(id.as_str())).collect();
    if !missing.is_empty() {
        bail!(
            "remote did not return {} of the {} blob(s) requested from /v1/blobs/fetch: {:?} — \
             pull aborted (a manifest referencing an absent blob would be incomplete)",
            missing.len(),
            ids.len(),
            missing,
        );
    }
    Ok(got.len())
}

/// Every stage id the given ops produce — used to fetch the attestations
/// keyed to those stages (#916). Intentionally id-only (unlike
/// `pull_objects`'s `(sig, stage)` pairing for content): the attestation log
/// is indexed by raw stage id server-side (`AttestationLog::by_stage`), not
/// by `(sig, stage)` variant.
fn produced_stage_ids(ops: &[OperationRecord]) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for rec in ops {
        for sid in rec.produces.stage_ids() {
            out.insert(sid);
        }
    }
    out
}

/// A stage whose attestations were skipped because the remote genuinely
/// doesn't have it (see [`sync_attestations`]).
struct AttestationSyncSkip {
    stage_id: String,
    reason: String,
}

/// A stage whose attestation fetch failed for a reason that is NOT a
/// confirmed absence (see [`sync_attestations`]).
struct AttestationSyncFailure {
    stage_id: String,
    detail: String,
}

/// Outcome of [`sync_attestations`]. Best-effort by construction: a per-stage
/// problem lands in `skipped` or `failures` rather than aborting the whole
/// sync, so the caller always gets back everything that DID succeed.
struct AttestationSyncOutcome {
    added: usize,
    skipped: Vec<AttestationSyncSkip>,
    failures: Vec<AttestationSyncFailure>,
}

/// Pull the attestations keyed to `stage_ids` (#916) — the trusted
/// `lex-hub-ci` verdicts and any other evidence the remote holds. Without
/// this a puller never sees them, and a `policy require-attestation` gate
/// can't be checked locally.
///
/// #1030: on a large real history, one stage among tens of thousands can be
/// genuinely absent from the remote's store — GC'd, never persisted, or a
/// stranded pre-#992 reference (a `ChangeEffectSig`-class op names a
/// `(sig, stage)` pair no store ever actually held content for). The
/// server's `stage_attestations_handler` 404s in exactly that case — and
/// only that case: it 404s iff `Store::get_metadata` can't find the stage
/// under any sig, and every other failure it turns into a 5xx. So a `404`
/// here is trustworthy evidence of genuine absence, mirroring the
/// skip-vs-fail distinction #868 (PR #1012) established for replay's parent
/// reconstruction: skip-and-report an absent stage's attestations, but never
/// swallow a real error (corrupt body, 5xx, transport failure, a response
/// that isn't a valid `Attestation`) as if it were one — those land in
/// `failures` instead of being silently skipped.
///
/// This function itself never fails the pull: it has no side effect on the
/// op/stage/intent DAG (unlike [`pull_objects`]), so treating one stage's
/// attestation trouble as fatal to the whole pull — discarding a fully
/// transferred 136k-op batch over one 404 — was the #1030 bug. `cmd_op_pull`
/// now calls this only *after* the branch head has already advanced, and
/// decides for itself whether a non-empty `failures` should fail the command
/// (it does — "fail loud" — but only after everything that succeeded is
/// already committed).
fn sync_attestations(
    remote: &str,
    stage_ids: &std::collections::BTreeSet<String>,
    store: &Store,
    token: Option<&str>,
) -> Result<AttestationSyncOutcome> {
    let attestation_log = lex_vcs::AttestationLog::open(store.root())?;
    let mut outcome = AttestationSyncOutcome {
        added: 0,
        skipped: Vec::new(),
        failures: Vec::new(),
    };
    for sid in stage_ids {
        let path = format!("/v1/stage/{sid}/attestations");
        let fetched: std::result::Result<serde_json::Value, SyncError> = request_json(
            remote, &path, None, token, Retry::Idempotent, &RetryPolicy::from_env(),
        );
        let v = match fetched {
            Ok(v) => v,
            Err(SyncError::Status { status: 404, .. }) => {
                outcome.skipped.push(AttestationSyncSkip {
                    stage_id: sid.clone(),
                    reason: "stage unknown to remote (genuinely absent — GC'd, never \
                             persisted, or a stranded pre-#992 reference)"
                        .to_string(),
                });
                continue;
            }
            Err(e) => {
                outcome.failures.push(AttestationSyncFailure {
                    stage_id: sid.clone(),
                    detail: e.to_string(),
                });
                continue;
            }
        };
        let Some(arr) = v.get("attestations").and_then(|a| a.as_array()) else {
            continue;
        };
        for av in arr {
            let att: lex_vcs::Attestation = match serde_json::from_value(av.clone()) {
                Ok(a) => a,
                Err(e) => {
                    outcome.failures.push(AttestationSyncFailure {
                        stage_id: sid.clone(),
                        detail: format!("malformed attestation in response: {e}"),
                    });
                    continue;
                }
            };
            if attestation_log.get(&att.attestation_id)?.is_none() {
                attestation_log.put(&att)?;
                outcome.added += 1;
            }
        }
    }
    Ok(outcome)
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
    let received = fetch_ops_paginated(&remote, &branch, cutoff.as_deref(), limit, token.as_deref())?;

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

    // Content: fetch + store the stage (code) and intent blobs the pulled
    // ops reference, so the pulled op-log actually renders and replays.
    // Attestation sync (#916) happens later, deliberately after the branch
    // head has advanced — see `sync_attestations`'s doc (#1030).
    let (_stages_pulled, _intents_pulled) =
        pull_objects(&remote, &received, &store, token.as_deref())?;

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

    // #1007 PR 5: fetch + re-hash the blobs any `SetFiles` op in this delta
    // (or the manifest the new tip would inherit) names, BEFORE the branch
    // head advances — a corrupt or incomplete blob must never become a
    // "successfully pulled" head. The ops themselves are already in the
    // local log at this point (harmless if we now bail: they're
    // content-addressed and this pull is simply re-attempted later).
    let blobs_pulled = pull_blobs(&remote, &received, &new_tip, &store, token.as_deref())?;

    // Fast-forward: advance the branch head to the new tip.
    // `Store::set_branch_head_op` is `pub(crate)`, so we go through
    // the JSON file directly. (#262 will replace this with a CAS
    // path; for now we rely on the single-writer invariant.)
    fast_forward_branch_head(&root, &branch, &new_tip)
        .with_context(|| format!("advancing branch head to {new_tip}"))?;

    // #930 P2b-1: fetch + store the committed lex.lock for the new tip, so the
    // write-time gate here can resolve this head's pinned dependencies the
    // same way the remote did. A dependency-free head has no lock to fetch.
    {
        let resp = post_json(
            &remote,
            "/v1/locks/fetch",
            &serde_json::json!({ "head_ops": [new_tip] }),
            token.as_deref(),
        )?;
        if let Some(locks) = resp.get("locks").and_then(|l| l.as_object()) {
            for (head, toml) in locks {
                if let Some(t) = toml.as_str() {
                    store.set_committed_lock(head, t)?;
                }
            }
        }
    }

    // #949 phase 1: fetch every issue the remote holds (open issues aren't
    // reachable from pulled ops) and store the ones missing locally.
    {
        let listed = get_json(&remote, "/v1/issues/list", token.as_deref())?;
        let ids: Vec<String> = listed
            .get("ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !ids.is_empty() {
            let ilog = lex_vcs::IssueLog::open(store.root())?;
            let want: Vec<String> = ids
                .into_iter()
                .filter(|id| !matches!(ilog.get(id), Ok(Some(_))))
                .collect();
            if !want.is_empty() {
                let resp = post_json(
                    &remote,
                    "/v1/issues/fetch",
                    &serde_json::json!({ "ids": want }),
                    token.as_deref(),
                )?;
                if let Some(arr) = resp.get("issues").and_then(|v| v.as_array()) {
                    for v in arr {
                        if let Ok(issue) = serde_json::from_value::<lex_vcs::Issue>(v.clone()) {
                            ilog.put(&issue)?;
                        }
                    }
                }
            }
        }
    }

    // Attestation sync (#916), run only now that ops + stages + the branch
    // head are all committed above. A stage whose attestations are
    // unreachable must never re-discard a pull that has already fully
    // landed — that was #1030. `sync_attestations` itself never fails the
    // pull; this function decides whether a non-empty `failures` should.
    let produced_stages = produced_stage_ids(&received);
    let att_outcome = sync_attestations(&remote, &produced_stages, &store, token.as_deref())?;
    let skipped_json: Vec<serde_json::Value> = att_outcome
        .skipped
        .iter()
        .map(|s| serde_json::json!({"stage_id": s.stage_id, "reason": s.reason}))
        .collect();

    if !att_outcome.failures.is_empty() {
        let failed_json: Vec<serde_json::Value> = att_outcome
            .failures
            .iter()
            .map(|f| serde_json::json!({"stage_id": f.stage_id, "detail": f.detail}))
            .collect();
        let failed_count = att_outcome.failures.len();
        let joined_details = att_outcome
            .failures
            .iter()
            .map(|f| format!("{}: {}", f.stage_id, f.detail))
            .collect::<Vec<_>>()
            .join("; ");
        let envelope = serde_json::json!({
            "remote": remote,
            "branch": branch,
            "received": received.len(),
            "added": added,
            "attestations_added": att_outcome.added,
            "attestations_skipped": skipped_json,
            "attestations_failed": failed_json,
            "fast_forwarded_to": new_tip,
            "error": "AttestationSyncFailed",
            "remark": "ops and stages were committed and the branch head advanced to the new \
                       tip; only attestation sync for the listed stage(s) failed. Those \
                       failures are NOT confirmed absences (a genuinely absent stage is \
                       reported in attestations_skipped, not here), so they are not silently \
                       swallowed. Attestation sync is only attempted for stages produced by the \
                       ops received in *this* pull, so a plain retry now that the branch is at \
                       this tip will report \"nothing to pull\" rather than retrying just the \
                       failed attestation(s) — a dedicated resync path is a follow-up.",
        });
        let env_clone = envelope.clone();
        acli::emit_or_text("op-pull", envelope, fmt, move || {
            eprintln!("{}", serde_json::to_string_pretty(&env_clone).unwrap());
        });
        bail!(
            "attestation sync failed for {failed_count} stage(s); ops and stages were \
             already committed and the branch head already advanced to {new_tip} (a plain \
             retry will not redo that transfer): {joined_details}"
        );
    }

    let data = serde_json::json!({
        "remote": remote,
        "branch": branch,
        "received": received.len(),
        "added": added,
        "blobs_pulled": blobs_pulled,
        "attestations_added": att_outcome.added,
        "attestations_skipped": skipped_json,
        "fast_forwarded_to": new_tip,
    });
    let total = received.len();
    let remote_text = remote.clone();
    let branch_text = branch.clone();
    let new_tip_text = new_tip.clone();
    let attestations_added = att_outcome.added;
    let attestations_skipped = att_outcome.skipped.len();
    acli::emit_or_text("op-pull", data, fmt, move || {
        let att = if attestations_added > 0 {
            format!(", {attestations_added} attestation(s)")
        } else {
            String::new()
        };
        let skip = if attestations_skipped > 0 {
            format!(
                ", {attestations_skipped} attestation fetch(es) skipped (stage genuinely \
                 absent on remote)"
            )
        } else {
            String::new()
        };
        println!(
            "pulled {total} ops from {remote_text} on branch `{branch_text}`: \
             {added} new{att}{skip}, branch advanced to {new_tip_text}"
        );
    });
    Ok(())
}

/// Fetch a delta from `<remote>/v1/ops/since`. Returns the records
/// in the order the server sent them (oldest-first by contract).
/// Ops requested per page when pulling. Op records are small (they carry
/// stage/intent *ids*, not content), so a page of this many stays far under
/// any response cap; content is fetched separately by `pull_objects`.
const OPS_PAGE: usize = 1000;

/// One page of `/v1/ops/since`: ops reachable from `branch.head_op` but not
/// from `after`, oldest-first, capped at `limit`, plus the server's
/// `X-Lex-Next-Cursor` when more remain (#971). `cursor`, when set, resumes
/// the pull where the previous page stopped; servers that predate cursors
/// ignore it and answer from `after`, so both are always sent.
fn fetch_ops_since(
    remote: &str,
    branch: &str,
    after: Option<&str>,
    cursor: Option<&str>,
    limit: Option<usize>,
    token: Option<&str>,
) -> Result<(Vec<OperationRecord>, Option<String>)> {
    let mut path = format!("/v1/ops/since?branch={branch}");
    if let Some(a) = after { path.push_str(&format!("&after={a}")); }
    if let Some(c) = cursor { path.push_str(&format!("&cursor={c}")); }
    if let Some(n) = limit { path.push_str(&format!("&limit={n}")); }
    // Read-only and cursor-addressed, so safe to retry.
    Ok(request_json_with_header(
        remote,
        &path,
        None,
        token,
        Retry::Idempotent,
        &RetryPolicy::from_env(),
        Some("X-Lex-Next-Cursor"),
    )?)
}

/// Pull the full op delta by paging through `/v1/ops/since`, so an
/// arbitrarily long history transfers without a single response exceeding
/// the read cap. `total_limit` (from `--limit`) caps the overall number
/// pulled; `None` means all. Pages are oldest-first and stitched in order.
fn fetch_ops_paginated(
    remote: &str,
    branch: &str,
    start_after: Option<&str>,
    total_limit: Option<usize>,
    token: Option<&str>,
) -> Result<Vec<OperationRecord>> {
    page_ops(start_after, total_limit, OPS_PAGE, |after, cursor, want| {
        fetch_ops_since(remote, branch, after, cursor, Some(want), token)
    })
}

/// The paging loop behind [`fetch_ops_paginated`], over any page source
/// `fetch(after, cursor, want) -> (page, next_cursor)`.
///
/// Each request carries `after=<last op received>` (what every server
/// understands) and, once the server has sent one, the `cursor` it handed
/// back, which lets a #971 server resume in O(page) instead of re-deriving
/// the delta from `after`. A server that has sent cursors signals the end
/// by omitting one; an older server never sends any, so for it the end is
/// a short or empty page, as before.
fn page_ops<F>(
    start_after: Option<&str>,
    total_limit: Option<usize>,
    page_size: usize,
    mut fetch: F,
) -> Result<Vec<OperationRecord>>
where
    F: FnMut(Option<&str>, Option<&str>, usize) -> Result<(Vec<OperationRecord>, Option<String>)>,
{
    let mut all: Vec<OperationRecord> = Vec::new();
    let mut after: Option<String> = start_after.map(String::from);
    let mut cursor: Option<String> = None;
    loop {
        let want = match total_limit {
            Some(t) if t <= all.len() => break,
            Some(t) => (t - all.len()).min(page_size),
            None => page_size,
        };
        let (page, next) = fetch(after.as_deref(), cursor.as_deref(), want)?;
        if page.is_empty() {
            break;
        }
        after = Some(page.last().unwrap().op_id.clone());
        let short = page.len() < want;
        all.extend(page);
        if short || (cursor.is_some() && next.is_none()) {
            break; // last page
        }
        cursor = next;
    }
    Ok(all)
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

#[cfg(test)]
mod unsatisfiable_pair_tests {
    use super::unsatisfiable_pair_message;

    #[test]
    fn a_hub_422_unsatisfiable_pair_prints_the_pair_and_the_hint() {
        let body = serde_json::json!({
            "error": "UnsatisfiablePair",
            "detail": {
                "sig_id": "old", "stage_id": "st", "filed_under": "new",
                "hint": "republish from source to retire the stranded entry (#995)",
            }
        });
        let msg = unsatisfiable_pair_message(422, &body).expect("recognised");
        for want in ["old", "st", "new", "hint: republish from source to retire the stranded entry (#995)"] {
            assert!(msg.contains(want), "missing {want:?} in: {msg}");
        }
    }

    #[test]
    fn other_errors_are_not_mistaken_for_it() {
        let body = serde_json::json!({ "error": "NonFastForward", "detail": {} });
        assert!(unsatisfiable_pair_message(422, &body).is_none());
        let body = serde_json::json!({ "error": "UnsatisfiablePair", "detail": {} });
        assert!(unsatisfiable_pair_message(500, &body).is_none());
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;
    use lex_vcs::{Operation, OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn ops(n: usize) -> Vec<OperationRecord> {
        (0..n)
            .map(|i| {
                let kind = OperationKind::AddFunction {
                    sig_id: format!("s{i}"),
                    stage_id: format!("t{i}"),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                    in_file: None,
                };
                let t = StageTransition::Create { sig_id: format!("s{i}"), stage_id: format!("t{i}") };
                OperationRecord::new(Operation::new(kind, []), t)
            })
            .collect()
    }

    fn pos_after(all: &[OperationRecord], after: Option<&str>) -> usize {
        after.map_or(0, |a| all.iter().position(|r| r.op_id == a).unwrap() + 1)
    }

    /// A pre-#971 server: answers from `after`, never sends a cursor.
    #[test]
    fn legacy_server_pages_by_after_until_a_short_page() {
        let all = ops(10);
        let mut calls = Vec::new();
        let got = page_ops(None, None, 3, |after, cursor, want| {
            assert!(cursor.is_none());
            let from = pos_after(&all, after);
            calls.push(from);
            Ok((all[from..(from + want).min(all.len())].to_vec(), None))
        })
        .unwrap();
        assert_eq!(got.iter().map(|r| &r.op_id).collect::<Vec<_>>(), all.iter().map(|r| &r.op_id).collect::<Vec<_>>());
        assert_eq!(calls, vec![0, 3, 6, 9]);
    }

    /// A #971 server: the client echoes the cursor (and still sends
    /// `after`), and stops when the server stops sending one — no trailing
    /// empty request when the delta is an exact multiple of the page.
    #[test]
    fn cursor_server_is_resumed_by_cursor_and_ends_without_an_empty_page() {
        let all = ops(9);
        let mut calls = Vec::new();
        let got = page_ops(None, None, 3, |after, cursor, want| {
            let from = match cursor {
                Some(c) => c.parse::<usize>().unwrap(),
                None => pos_after(&all, after),
            };
            assert_eq!(from, pos_after(&all, after), "`after` must track the cursor for old servers");
            calls.push(from);
            let end = (from + want).min(all.len());
            let next = (end < all.len()).then(|| end.to_string());
            Ok((all[from..end].to_vec(), next))
        })
        .unwrap();
        assert_eq!(got.len(), 9);
        assert_eq!(calls, vec![0, 3, 6]);
    }

    #[test]
    fn total_limit_caps_the_pull() {
        let all = ops(10);
        let got = page_ops(None, Some(5), 3, |after, _, want| {
            let from = pos_after(&all, after);
            let end = (from + want).min(all.len());
            Ok((all[from..end].to_vec(), (end < all.len()).then(|| end.to_string())))
        })
        .unwrap();
        assert_eq!(got.len(), 5);
    }
}
