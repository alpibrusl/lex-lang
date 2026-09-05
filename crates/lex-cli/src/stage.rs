//! `lex stage`: stage inspection, candidates, pinning and triage decisions.

use super::*;

/// `lex stage <stage_id>` — print metadata + canonical AST + status.
/// `lex stage <stage_id> --attestations` — list every attestation
/// for the stage, newest-first by timestamp. CLI mirror of
/// `GET /v1/stage/<id>` and `GET /v1/stage/<id>/attestations`.
pub(super) fn cmd_stage(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    // `lex stage pin|defer|block|unblock <id> ...` — human triage
    // actions (#172). Detect them as a leading positional so the
    // existing `lex stage <id>` and `lex stage <id> --attestations`
    // shapes keep working unchanged.
    if let Some(action) = rest.first().map(String::as_str) {
        match action {
            "pin" => return cmd_stage_pin(fmt, &root, &rest[1..]),
            "defer" => return cmd_stage_decision(fmt, &root, &rest[1..], StageDecision::Defer),
            "block" => return cmd_stage_decision(fmt, &root, &rest[1..], StageDecision::Block),
            "unblock" => return cmd_stage_decision(fmt, &root, &rest[1..], StageDecision::Unblock),
            // #294: multi-agent coordination.
            "candidates" => return cmd_stage_candidates(fmt, &root, &rest[1..]),
            "promote-candidate" => return cmd_stage_promote_candidate(fmt, &root, &rest[1..]),
            _ => {}
        }
    }
    let attestations_mode = rest.iter().any(|a| a == "--attestations");
    let id = rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex stage <stage_id> [--attestations]"))?;
    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;

    if attestations_mode {
        // 404-equivalent: refuse to list against an unknown stage so
        // callers can't silently get an empty list for a typo.
        store
            .get_metadata(id)
            .with_context(|| format!("unknown stage `{id}`"))?;
        let log = store.attestation_log()?;
        let mut listing = log.list_for_stage(id)?;
        listing.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
        let data = serde_json::json!({
            "stage_id": id,
            "attestations": serde_json::to_value(&listing)?,
        });
        acli::emit_or_text("stage", data, fmt, move || {
            if listing.is_empty() {
                println!("(no attestations)");
                return;
            }
            for a in &listing {
                let kind = match &a.kind {
                    lex_vcs::AttestationKind::TypeCheck => "TypeCheck".to_string(),
                    lex_vcs::AttestationKind::EffectAudit => "EffectAudit".to_string(),
                    lex_vcs::AttestationKind::Examples { count, .. } => {
                        format!("Examples({count})")
                    }
                    lex_vcs::AttestationKind::Spec { spec_id, .. } => {
                        format!("Spec({spec_id})")
                    }
                    lex_vcs::AttestationKind::DiffBody { input_count, .. } => {
                        format!("DiffBody({input_count})")
                    }
                    lex_vcs::AttestationKind::SandboxRun { effects } => {
                        let joined: Vec<&str> = effects.iter().map(String::as_str).collect();
                        format!("SandboxRun([{}])", joined.join(","))
                    }
                    lex_vcs::AttestationKind::Override { actor, .. } => {
                        format!("Override({actor})")
                    }
                    lex_vcs::AttestationKind::Defer { actor, .. } => {
                        format!("Defer({actor})")
                    }
                    lex_vcs::AttestationKind::Block { actor, .. } => {
                        format!("Block({actor})")
                    }
                    lex_vcs::AttestationKind::Unblock { actor, .. } => {
                        format!("Unblock({actor})")
                    }
                    lex_vcs::AttestationKind::Trace {
                        run_id,
                        root_target,
                    } => {
                        format!("Trace({root_target}@{run_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::ProducerBlock { tool_id, .. } => {
                        format!("ProducerBlock({tool_id})")
                    }
                    lex_vcs::AttestationKind::ProducerUnblock { tool_id, .. } => {
                        format!("ProducerUnblock({tool_id})")
                    }
                    lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => {
                        format!("RepairHint({failed_op_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::RepairAttempt {
                        hint_id, outcome, ..
                    } => {
                        format!("RepairAttempt({outcome}, {hint_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::ProducerTrust {
                        tool_id,
                        score_thousandths,
                        ..
                    } => {
                        format!(
                            "ProducerTrust({tool_id}, {:.3})",
                            *score_thousandths as f64 / 1000.0
                        )
                    }
                    lex_vcs::AttestationKind::TrustWaived {
                        producer, kind_tag, ..
                    } => {
                        format!("TrustWaived({producer}/{kind_tag})")
                    }
                    lex_vcs::AttestationKind::CapsuleInstall {
                        artifact, signer, ..
                    } => {
                        format!("CapsuleInstall({artifact} by {signer:.12}…)")
                    }
                };
                let result = match &a.result {
                    lex_vcs::AttestationResult::Passed => "passed".to_string(),
                    lex_vcs::AttestationResult::Failed { detail } => format!("failed: {detail}"),
                    lex_vcs::AttestationResult::Inconclusive { detail } => {
                        format!("inconclusive: {detail}")
                    }
                };
                println!(
                    "{}\t{}\t{}\tby={}@{}",
                    a.timestamp, kind, result, a.produced_by.tool, a.produced_by.version,
                );
            }
        });
        return Ok(());
    }

    // Default: stage info, mirroring `GET /v1/stage/<id>`.
    let meta = store.get_metadata(id)?;
    let ast = store.get_ast(id)?;
    let status = format!("{:?}", store.get_status(id)?).to_lowercase();
    let v = serde_json::json!({
        "metadata": serde_json::to_value(&meta)?,
        "ast": serde_json::to_value(&ast)?,
        "status": status,
    });
    acli::emit_or_text("stage", v.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    });
    Ok(())
}

/// Resolve and validate the actor for a triage action.
/// Combines `--actor` and `LEX_TEA_USER` (in that order), and
/// when `<root>/users.json` exists requires the resulting name
/// to be in the file. Returns a printable error mentioning the
/// command verb so the user can see which surface refused them.
pub(super) fn resolve_actor(root: &std::path::Path, supplied: Option<String>, verb: &str) -> Result<String> {
    let actor = supplied
        .or_else(|| std::env::var("LEX_TEA_USER").ok())
        .ok_or_else(|| {
            anyhow!("lex stage {verb}: actor unknown — pass --actor NAME or set LEX_TEA_USER")
        })?;
    if let Some(users) = lex_store::users::load(root)
        .with_context(|| format!("reading users.json at {}", root.display()))?
    {
        if !users.knows(&actor) {
            bail!(
                "lex stage {verb}: actor `{actor}` not listed in {}/users.json",
                root.display()
            );
        }
    }
    Ok(actor)
}

/// `lex stage pin <id> --reason "..." [--actor <name>]` —
/// human override (#172, lex-tea v3a). Activates the stage and
/// records an `Override` attestation alongside whatever
/// existing attestations the stage already has. The pin
/// itself is auditable: `lex attest filter --kind override`
/// returns every override the human(s) have issued.
///
/// `actor` defaults to `$LEX_TEA_USER`; falling back errors so
/// `lex stage candidates <sig_id> [--store DIR]` (#294). Lists
/// every live `Candidate` op for the sig — those not yet
/// referenced as winner or in `supersedes` by any `Promote`.
/// Sorted by op_id for reproducibility.
pub(super) fn cmd_stage_candidates(fmt: &OutputFormat, root: &std::path::Path, rest: &[String]) -> Result<()> {
    let sig_id = rest
        .first()
        .ok_or_else(|| anyhow!("usage: lex stage candidates <sig_id> [--store DIR]"))?;
    let store = Store::open(root)?;
    let candidates = store.list_candidates(sig_id)?;
    let data = serde_json::json!({
        "sig_id": sig_id,
        "candidates": &candidates,
        "count": candidates.len(),
    });
    let sig_for_text = sig_id.clone();
    let printable = candidates.clone();
    acli::emit_or_text("stage-candidates", data, fmt, move || {
        if printable.is_empty() {
            println!("(no live candidates for `{sig_for_text}`)");
            return;
        }
        println!("{} candidate(s) for `{sig_for_text}`:", printable.len());
        for c in &printable {
            let intent = c.intent_id.as_deref().unwrap_or("(none)");
            println!(
                "  op_id={:.16}…  stage_id={:.16}…  intent={:.16}…",
                c.op_id, c.stage_id, intent
            );
        }
    });
    Ok(())
}

/// `lex stage promote-candidate <candidate_op_id> [--branch B]
/// [--store DIR]` (#294). Emits a `Promote` op advancing the
/// branch head with the candidate's stage. Every other live
/// candidate for the same sig is listed in `supersedes` so the
/// op log explicitly records the bake-off.
pub(super) fn cmd_stage_promote_candidate(
    fmt: &OutputFormat,
    root: &std::path::Path,
    rest: &[String],
) -> Result<()> {
    let mut op_id: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--branch needs a name"))?
                        .clone(),
                );
            }
            other if !other.starts_with("--") => {
                if op_id.is_some() {
                    bail!("usage: lex stage promote-candidate <op_id> [--branch B]");
                }
                op_id = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let op_id = op_id.ok_or_else(|| {
        anyhow!("usage: lex stage promote-candidate <op_id> [--branch B] [--store DIR]")
    })?;
    let store = Store::open(root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    let new_op_id = store.promote_candidate(&branch, &op_id)?;
    let data = serde_json::json!({
        "promoted_candidate": op_id,
        "new_op_id": new_op_id,
        "branch": branch,
    });
    let candidate_for_text = op_id.clone();
    let new_id_for_text = new_op_id.clone();
    let branch_for_text = branch.clone();
    acli::emit_or_text("stage-promote-candidate", data, fmt, move || {
        println!("promoted candidate `{candidate_for_text}` on `{branch_for_text}`");
        println!("  new op_id: {new_id_for_text}");
    });
    Ok(())
}

/// a pin can't land anonymously. When `<store>/users.json`
/// exists, the resolved name must be in the file (v3d, #172).
pub(super) fn cmd_stage_pin(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut id: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reason" => {
                reason = args.get(i + 1).cloned();
                i += 2;
            }
            "--actor" => {
                actor = args.get(i + 1).cloned();
                i += 2;
            }
            other if id.is_none() => {
                id = Some(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let id = id.ok_or_else(|| {
        anyhow!("usage: lex stage pin <stage_id> --reason \"...\" [--actor NAME]")
    })?;
    let reason = reason.ok_or_else(|| {
        anyhow!("lex stage pin: --reason required (overrides need a paper trail)")
    })?;
    let actor = resolve_actor(root, actor, "pin")?;

    let store =
        Store::open(root).with_context(|| format!("opening store at {}", root.display()))?;
    // Verify the stage exists; refuse to pin something that's not
    // even there so a typo can't accidentally activate the wrong
    // sig later.
    let _ = store
        .get_metadata(&id)
        .with_context(|| format!("unknown stage `{id}`"))?;

    // Refuse to pin a blocked stage. The block is only meaningful
    // if it actually stops the activation it's supposed to prevent.
    let log = store.attestation_log()?;
    let existing = log.list_for_stage(&id)?;
    if lex_vcs::is_stage_blocked(&existing) {
        bail!(
            "lex stage pin: stage `{id}` is blocked — run `lex stage unblock {id} --reason \"...\"` first"
        );
    }

    // Activate first (the actual override action), then record the
    // audit. Order matters: if activate fails, no audit is written;
    // if audit fails after a successful activate, the user retries
    // and the attestation_id is content-addressed so re-puts dedup.
    store
        .activate(&id)
        .with_context(|| format!("activate stage `{id}`"))?;

    let producer = lex_vcs::ProducerDescriptor {
        tool: "lex stage pin".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    };
    let attestation = lex_vcs::Attestation::new(
        id.clone(),
        None,
        None,
        lex_vcs::AttestationKind::Override {
            actor: actor.clone(),
            reason: reason.clone(),
            target_attestation_id: None,
        },
        // Override is a *fact* about the human's choice, not a
        // pass/fail of code. Use Passed for "the override was
        // recorded successfully" — Failed/Inconclusive don't
        // apply to administrative actions.
        lex_vcs::AttestationResult::Passed,
        producer,
        None,
    );
    let log = store.attestation_log()?;
    log.put(&attestation)?;

    let data = serde_json::json!({
        "pinned": &id,
        "actor": &actor,
        "reason": &reason,
        "attestation_id": &attestation.attestation_id,
    });
    let id_for_text = id.clone();
    let actor_for_text = actor.clone();
    acli::emit_or_text("stage", data, fmt, move || {
        println!("→ pinned `{id_for_text:.16}…` (actor: {actor_for_text})");
    });
    Ok(())
}

/// Triage decisions a human can record on a stage. Mirrors the
/// `Defer`/`Block`/`Unblock` `AttestationKind` variants.
#[derive(Clone, Copy)]
pub(super) enum StageDecision {
    Defer,
    Block,
    Unblock,
}

impl StageDecision {
    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Defer => "defer",
            Self::Block => "block",
            Self::Unblock => "unblock",
        }
    }

    pub(super) fn past(self) -> &'static str {
        match self {
            Self::Defer => "deferred",
            Self::Block => "blocked",
            Self::Unblock => "unblocked",
        }
    }
}

/// `lex stage <defer|block|unblock> <id> --reason "..." [--actor NAME]`
/// — human triage actions (#172, lex-tea v3b).
///
/// Defer/Block/Unblock all record an attestation against the stage
/// without changing its status. Block additionally makes future
/// `lex stage pin` calls refuse until an `unblock` is recorded.
/// The append-only attestation log makes the full triage history
/// queryable via `lex attest filter --kind block` etc.
pub(super) fn cmd_stage_decision(
    fmt: &OutputFormat,
    root: &std::path::Path,
    args: &[String],
    decision: StageDecision,
) -> Result<()> {
    let mut id: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reason" => {
                reason = args.get(i + 1).cloned();
                i += 2;
            }
            "--actor" => {
                actor = args.get(i + 1).cloned();
                i += 2;
            }
            other if id.is_none() => {
                id = Some(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let verb = decision.verb();
    let id = id.ok_or_else(|| {
        anyhow!("usage: lex stage {verb} <stage_id> --reason \"...\" [--actor NAME]")
    })?;
    let reason = reason.ok_or_else(|| {
        anyhow!("lex stage {verb}: --reason required (triage decisions need a paper trail)")
    })?;
    let actor = resolve_actor(root, actor, verb)?;

    let store =
        Store::open(root).with_context(|| format!("opening store at {}", root.display()))?;
    let _ = store
        .get_metadata(&id)
        .with_context(|| format!("unknown stage `{id}`"))?;

    let kind = match decision {
        StageDecision::Defer => lex_vcs::AttestationKind::Defer {
            actor: actor.clone(),
            reason: reason.clone(),
        },
        StageDecision::Block => lex_vcs::AttestationKind::Block {
            actor: actor.clone(),
            reason: reason.clone(),
        },
        StageDecision::Unblock => lex_vcs::AttestationKind::Unblock {
            actor: actor.clone(),
            reason: reason.clone(),
        },
    };
    let producer = lex_vcs::ProducerDescriptor {
        tool: format!("lex stage {verb}"),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    };
    let attestation = lex_vcs::Attestation::new(
        id.clone(),
        None,
        None,
        kind,
        lex_vcs::AttestationResult::Passed,
        producer,
        None,
    );
    let log = store.attestation_log()?;
    log.put(&attestation)?;

    let data = serde_json::json!({
        "stage_id": &id,
        "decision": verb,
        "actor": &actor,
        "reason": &reason,
        "attestation_id": &attestation.attestation_id,
    });
    let id_for_text = id.clone();
    let actor_for_text = actor.clone();
    let past = decision.past();
    acli::emit_or_text("stage", data, fmt, move || {
        println!("→ {past} `{id_for_text:.16}…` (actor: {actor_for_text})");
    });
    Ok(())
}
