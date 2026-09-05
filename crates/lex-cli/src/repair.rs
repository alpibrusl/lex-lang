//! `lex repair`: structured-error driven transforms, explicit (`--transform`) and LLM-driven (`--apply`).

use super::*;

/// `lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]`
/// (#281). Reads the latest `RepairHint` for the failed op_id and
/// — in `--apply` mode — executes a typed transform supplied as
/// JSON. Emits a `RepairAttempt` attestation with the outcome.
///
/// Slice 2a ships the explicit-transform path; the LLM-driven
/// path (`--apply` without `--transform`) follows in slice 2b.
pub(super) fn cmd_repair(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut op_id: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let mut apply = false;
    let mut transform_json: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => {
                root = Some(PathBuf::from(
                    it.next().ok_or_else(|| anyhow!("--store needs a path"))?,
                ));
            }
            "--apply" => apply = true,
            "--transform" => {
                transform_json = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--transform needs a JSON payload"))?
                        .clone(),
                );
            }
            "--branch" => {
                branch = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--branch needs a name"))?
                        .clone(),
                );
            }
            other if !other.starts_with("--") => {
                if op_id.is_some() {
                    bail!("usage: lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]");
                }
                op_id = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let op_id = op_id.ok_or_else(|| {
        anyhow!(
            "usage: lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]"
        )
    })?;
    let root = root.unwrap_or_else(crate::default_store_root_pub);
    let store = Store::open(&root)?;

    if apply {
        let branch = branch.unwrap_or_else(|| store.current_branch());
        // With `--transform`: slice-2a behavior — execute exactly
        // what the agent provided. Without it: slice-2b — call
        // the LLM (or fixture) to generate the transform.
        return match transform_json {
            Some(t) => cmd_repair_apply(fmt, &store, &op_id, &branch, &t),
            None => cmd_repair_apply_llm(fmt, &store, &op_id, &branch),
        };
    }
    if transform_json.is_some() {
        bail!("`--transform` requires `--apply`");
    }
    cmd_repair_read(fmt, &store, &op_id)
}

pub(super) fn cmd_repair_read(fmt: &OutputFormat, store: &Store, op_id: &str) -> Result<()> {
    let attlog = store
        .attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let mut hits: Vec<lex_vcs::Attestation> = attlog
        .list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| {
            matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id, .. }
                if failed_op_id == op_id)
        })
        .collect();
    hits.sort_by_key(|a| a.timestamp);
    let latest = hits.last().cloned();
    let envelope = match latest {
        Some(a) => {
            let lex_vcs::AttestationKind::RepairHint {
                failed_op_id,
                errors,
                suggested_transform,
            } = &a.kind
            else {
                unreachable!()
            };
            serde_json::json!({
                "found": true,
                "failed_op_id": failed_op_id,
                "stage_id": a.stage_id,
                "attestation_id": a.attestation_id,
                "timestamp": a.timestamp,
                "errors": errors,
                "suggested_transform": suggested_transform,
            })
        }
        None => serde_json::json!({
            "found": false,
            "failed_op_id": op_id,
        }),
    };
    let op_id_owned = op_id.to_string();
    acli::emit_or_text("repair", envelope.clone(), fmt, || {
        if envelope["found"] == false {
            println!("no RepairHint found for op_id `{op_id_owned}`");
        } else {
            let n = envelope["errors"].as_array().map(|a| a.len()).unwrap_or(0);
            let stage = envelope["stage_id"].as_str().unwrap_or("?");
            println!("RepairHint for op_id `{op_id_owned}`:");
            println!("  stage:  {stage}");
            println!("  errors: {n}");
            println!(
                "  suggested_transform: {}",
                if envelope["suggested_transform"].is_null() {
                    "(none — supply one via `lex repair --apply --transform ...`)".to_string()
                } else {
                    envelope["suggested_transform"].to_string()
                }
            );
        }
    });
    Ok(())
}

/// `lex repair <op_id> --apply --transform '<json>'` — slice 2a.
///
/// Parses the transform payload (one of #280's four typed transforms)
/// and dispatches to the matching `Store::apply_*` method. The
/// outcome is recorded as a `RepairAttempt` attestation tied to the
/// original RepairHint's attestation_id so blame walks the repair
/// chain. Returns the new op_id (or pair, for ExtractFunction) on
/// success.
pub(super) fn cmd_repair_apply(
    fmt: &OutputFormat,
    store: &Store,
    failed_op_id: &str,
    branch: &str,
    transform_json: &str,
) -> Result<()> {
    // Find the hint we're attesting against. Required so the
    // RepairAttempt's `hint_id` field is meaningful; without one,
    // a repair has no target to record progress against.
    let attlog = store
        .attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let hint = attlog
        .list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| {
            matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id: f, .. }
                if f == failed_op_id)
        })
        .max_by_key(|a| a.timestamp)
        .ok_or_else(|| {
            anyhow!(
                "no RepairHint exists for op_id `{failed_op_id}` — \
             a hint is required to apply a repair"
            )
        })?;
    let hint_attestation_id = hint.attestation_id.clone();
    let hint_stage_id = hint.stage_id.clone();

    let parsed: serde_json::Value = serde_json::from_str(transform_json)
        .with_context(|| format!("parsing --transform JSON: {transform_json}"))?;
    let kind = parsed
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("--transform JSON missing `kind` field"))?;

    let result = dispatch_repair_transform(store, branch, &parsed, kind);
    let (outcome, applied_op_id, error_detail) = match &result {
        Ok(op_ids) => ("passed".to_string(), op_ids.first().cloned(), None),
        Err(e) => ("failed".to_string(), None, Some(format!("{e}"))),
    };

    // Emit the RepairAttempt regardless of outcome — the audit
    // trail is load-bearing whether the attempt succeeded or not.
    let attempt = lex_vcs::Attestation::new(
        hint_stage_id.clone(),
        applied_op_id.clone(),
        None,
        lex_vcs::AttestationKind::RepairAttempt {
            hint_id: hint_attestation_id.clone(),
            outcome: outcome.clone(),
            applied_op_id: applied_op_id.clone(),
        },
        if outcome == "passed" {
            lex_vcs::AttestationResult::Passed
        } else {
            lex_vcs::AttestationResult::Failed {
                detail: error_detail.clone().unwrap_or_default(),
            }
        },
        repair_attempt_producer(),
        None,
    );
    attlog
        .put(&attempt)
        .map_err(|e| anyhow!("recording RepairAttempt: {e}"))?;

    let env = serde_json::json!({
        "outcome": outcome,
        "hint_id": hint_attestation_id,
        "applied_op_id": applied_op_id,
        "error": error_detail,
    });
    acli::emit_or_text("repair-apply", env.clone(), fmt, || {
        match env["outcome"].as_str() {
            Some("passed") => println!(
                "repair applied: new op_id = {}",
                env["applied_op_id"].as_str().unwrap_or("?")
            ),
            _ => println!("repair failed: {}", env["error"].as_str().unwrap_or("?")),
        }
    });

    // The command itself succeeded — it ran the transform and
    // recorded a RepairAttempt. The `outcome` field in the
    // envelope and the attestation's result carry the inner
    // success/failure. Exiting non-zero would have stdout emit
    // a second wrapper envelope, which we don't want.
    Ok(())
}

pub(super) fn repair_attempt_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex repair".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// `lex repair <op_id> --apply` (no `--transform`) — slice 2b.
///
/// Reads the latest `RepairHint` for the failed op_id, builds a
/// structured prompt describing the four typed transforms +
/// the failure context, asks the configured LLM for a single
/// transform JSON, then hands the response off to
/// [`cmd_repair_apply`]'s machinery.
///
/// # Test infrastructure
///
/// Tests can short-circuit the LLM call by setting the
/// `LEX_REPAIR_LLM_FIXTURE` env var to a path. The contents of
/// that file replace the live LLM response. This lets the
/// subprocess-based CLI tests assert end-to-end behavior without
/// any network dependency.
pub(super) fn cmd_repair_apply_llm(
    fmt: &OutputFormat,
    store: &Store,
    failed_op_id: &str,
    branch: &str,
) -> Result<()> {
    let attlog = store
        .attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let hint = attlog
        .list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| {
            matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id: f, .. }
                if f == failed_op_id)
        })
        .max_by_key(|a| a.timestamp)
        .ok_or_else(|| {
            anyhow!(
                "no RepairHint exists for op_id `{failed_op_id}` — \
             a hint is required to apply a repair"
            )
        })?;
    let lex_vcs::AttestationKind::RepairHint { errors, .. } = &hint.kind else {
        unreachable!()
    };

    let candidate_stage_id = &hint.stage_id;
    let candidate_stage = store
        .get_ast(candidate_stage_id)
        .map_err(|e| anyhow!("loading candidate stage `{candidate_stage_id}`: {e}"))?;
    let sig = lex_ast::sig_id(&candidate_stage)
        .ok_or_else(|| anyhow!("candidate stage has no sig_id"))?;
    let head = store
        .branch_head(branch)
        .map_err(|e| anyhow!("reading branch head: {e}"))?;
    let from_stage_id = head.get(&sig).cloned();
    let from_stage = match &from_stage_id {
        Some(id) => Some(
            store
                .get_ast(id)
                .map_err(|e| anyhow!("loading branch-head stage `{id}`: {e}"))?,
        ),
        None => None,
    };

    let prompt = build_repair_prompt(
        candidate_stage_id,
        &candidate_stage,
        from_stage_id.as_deref(),
        from_stage.as_ref(),
        errors,
    );
    let response = call_repair_llm(&prompt)?;
    let transform_json = response.trim().to_string();

    // Pre-validate that the response is at least parseable JSON
    // and has a `kind` field. A malformed response is recorded
    // as a `RepairAttempt` failure rather than propagated as
    // exit-non-zero — the LLM gave a bad answer; the command
    // itself processed correctly.
    let parse_err: Option<String> = match serde_json::from_str::<serde_json::Value>(&transform_json)
    {
        Ok(v) => {
            if v.get("kind").and_then(|x| x.as_str()).is_none() {
                Some("LLM response missing `kind` field".into())
            } else {
                None
            }
        }
        Err(e) => Some(format!("LLM response is not valid JSON: {e}")),
    };
    if let Some(reason) = parse_err {
        let attlog = store
            .attestation_log()
            .map_err(|e| anyhow!("opening attestation log: {e}"))?;
        let attempt = lex_vcs::Attestation::new(
            hint.stage_id.clone(),
            None,
            None,
            lex_vcs::AttestationKind::RepairAttempt {
                hint_id: hint.attestation_id.clone(),
                outcome: "failed".into(),
                applied_op_id: None,
            },
            lex_vcs::AttestationResult::Failed {
                detail: reason.clone(),
            },
            repair_attempt_producer(),
            None,
        );
        attlog
            .put(&attempt)
            .map_err(|e| anyhow!("recording RepairAttempt: {e}"))?;
        let env = serde_json::json!({
            "outcome": "failed",
            "hint_id": hint.attestation_id,
            "applied_op_id": serde_json::Value::Null,
            "error": reason,
        });
        let env_for_text = env.clone();
        acli::emit_or_text("repair-apply", env, fmt, move || {
            println!(
                "repair failed: {}",
                env_for_text["error"].as_str().unwrap_or("?")
            );
        });
        return Ok(());
    }

    cmd_repair_apply(fmt, store, failed_op_id, branch, &transform_json)
}

/// Build the prompt for the LLM repair call. Inlines the JSON
/// schemas for the four typed transforms so the model can choose
/// one without a separate spec fetch. Includes the candidate
/// stage (the one that didn't typecheck), the branch-head stage
/// (the one transforms should operate against), and the type
/// errors.
pub(super) fn build_repair_prompt(
    candidate_stage_id: &str,
    candidate_stage: &lex_ast::Stage,
    from_stage_id: Option<&str>,
    from_stage: Option<&lex_ast::Stage>,
    errors: &serde_json::Value,
) -> String {
    let candidate_json = serde_json::to_string_pretty(candidate_stage).unwrap_or_default();
    let from_json = from_stage
        .map(|s| serde_json::to_string_pretty(s).unwrap_or_default())
        .unwrap_or_else(|| "(no current branch-head stage for this sig)".into());
    let from_id_render = from_stage_id.unwrap_or("(none)");
    let errors_json = serde_json::to_string_pretty(errors).unwrap_or_default();

    format!(
        r#"You are a Lex type-error repair assistant. The user attempted a
typed transform; the resulting stage didn't typecheck. Suggest
exactly one typed AST transform that would fix the type errors.

# Available transforms (return JSON for ONE of these)

1) replace_match_arm — replace the body of one Match arm.
{{
  "kind": "replace_match_arm",
  "from_stage_id": "<branch-head stage_id>",
  "match_node": "<NodeId of the Match>",
  "arm_index": <0-based>,
  "new_body": <CExpr JSON>
}}

2) rename_local — rename a let-bound local (scope-aware).
{{
  "kind": "rename_local",
  "from_stage_id": "<branch-head stage_id>",
  "let_node": "<NodeId of the Let>",
  "new_name": "<identifier>"
}}

3) inline_let — eliminate `let x := v; body` by substituting v.
   v must be a literal/var/field-access/binop tree (no calls,
   no side effects).
{{
  "kind": "inline_let",
  "from_stage_id": "<branch-head stage_id>",
  "let_node": "<NodeId of the Let>"
}}

4) extract_function — extract a sub-expression into a new fn.
{{
  "kind": "extract_function",
  "from_stage_id": "<branch-head stage_id>",
  "expr_node": "<NodeId of the expr>",
  "spec": {{
    "name": "<new fn name>",
    "type_params": [],
    "params": [{{"name": "n", "type": {{"node": "Named", "name": "Int", "args": []}}}}],
    "return_type": {{"node": "Named", "name": "Int", "args": []}},
    "effects": []
  }}
}}

# Failure context

Branch-head stage_id (use this as `from_stage_id`):
{from_id_render}

Branch-head stage AST (the one the transform should operate on):
{from_json}

Candidate stage_id (the one that didn't typecheck): {candidate_stage_id}

Candidate stage AST (what the agent tried; informative only):
{candidate_json}

Type errors:
{errors_json}

# Response format

Output ONLY the JSON object for your chosen transform. No prose,
no markdown fences, no surrounding commentary.
"#
    )
}

/// Call the configured LLM. Test escape hatch: when
/// `LEX_REPAIR_LLM_FIXTURE` is set, read the response from that
/// file instead of calling the live model. Lets the subprocess-
/// based CLI tests assert end-to-end behavior without network.
pub(super) fn call_repair_llm(prompt: &str) -> Result<String> {
    if let Ok(path) = std::env::var("LEX_REPAIR_LLM_FIXTURE") {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("reading LEX_REPAIR_LLM_FIXTURE at `{path}`"));
    }
    lex_runtime::llm::cloud_complete(prompt).map_err(|e| anyhow!("LLM cloud_complete: {e}"))
}

/// Dispatch a `--transform` payload to the matching
/// `Store::apply_*` method. Returns the resulting op_ids
/// (singleton for the body transforms; pair for ExtractFunction).
pub(super) fn dispatch_repair_transform(
    store: &Store,
    branch: &str,
    payload: &serde_json::Value,
    kind: &str,
) -> Result<Vec<lex_vcs::OpId>> {
    match kind {
        "replace_match_arm" => {
            let from = require_str(payload, "from_stage_id")?;
            let match_node = require_str(payload, "match_node")?;
            let arm_index = payload
                .get("arm_index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("replace_match_arm: missing arm_index"))?
                as usize;
            let new_body: lex_ast::CExpr = serde_json::from_value(
                payload
                    .get("new_body")
                    .cloned()
                    .ok_or_else(|| anyhow!("replace_match_arm: missing new_body"))?,
            )
            .context("parsing new_body CExpr")?;
            let op = store.apply_replace_match_arm(
                branch,
                from,
                &lex_ast::NodeId(match_node.into()),
                arm_index,
                new_body,
            )?;
            Ok(vec![op])
        }
        "rename_local" => {
            let from = require_str(payload, "from_stage_id")?;
            let let_node = require_str(payload, "let_node")?;
            let new_name = require_str(payload, "new_name")?;
            let op = store.apply_rename_local(
                branch,
                from,
                &lex_ast::NodeId(let_node.into()),
                new_name,
            )?;
            Ok(vec![op])
        }
        "inline_let" => {
            let from = require_str(payload, "from_stage_id")?;
            let let_node = require_str(payload, "let_node")?;
            let op = store.apply_inline_let(branch, from, &lex_ast::NodeId(let_node.into()))?;
            Ok(vec![op])
        }
        "extract_function" => {
            let from = require_str(payload, "from_stage_id")?;
            let expr_node = require_str(payload, "expr_node")?;
            let spec: lex_ast::ExtractFnSpec = parse_extract_spec(
                payload
                    .get("spec")
                    .ok_or_else(|| anyhow!("extract_function: missing spec"))?,
            )?;
            let (add, modify) = store.apply_extract_function(
                branch,
                from,
                &lex_ast::NodeId(expr_node.into()),
                spec,
            )?;
            Ok(vec![add, modify])
        }
        other => bail!(
            "unknown transform kind `{other}` — valid kinds are \
             replace_match_arm | rename_local | inline_let | extract_function"
        ),
    }
}

pub(super) fn require_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("--transform JSON missing string field `{key}`"))
}

/// Parse an `ExtractFnSpec` from JSON. The schema mirrors the
/// `lex_ast::ExtractFnSpec` struct field-for-field; we hand-parse
/// rather than `serde_json::from_value` because `Param`/`TypeExpr`/
/// `Effect` go through `lex-ast`'s canonical-JSON form (which is
/// the same shape, but worth being explicit).
pub(super) fn parse_extract_spec(v: &serde_json::Value) -> Result<lex_ast::ExtractFnSpec> {
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("extract_function spec: missing name"))?
        .to_string();
    let type_params: Vec<String> = v
        .get("type_params")
        .map(|x| serde_json::from_value(x.clone()))
        .transpose()
        .context("extract_function spec.type_params")?
        .unwrap_or_default();
    let params: Vec<lex_ast::Param> = serde_json::from_value(
        v.get("params")
            .cloned()
            .ok_or_else(|| anyhow!("extract_function spec: missing params"))?,
    )
    .context("extract_function spec.params")?;
    let return_type: lex_ast::TypeExpr = serde_json::from_value(
        v.get("return_type")
            .cloned()
            .ok_or_else(|| anyhow!("extract_function spec: missing return_type"))?,
    )
    .context("extract_function spec.return_type")?;
    let effects: Vec<lex_ast::Effect> = v
        .get("effects")
        .map(|x| serde_json::from_value(x.clone()))
        .transpose()
        .context("extract_function spec.effects")?
        .unwrap_or_default();
    Ok(lex_ast::ExtractFnSpec {
        name,
        type_params,
        params,
        return_type,
        effects,
    })
}
