//! `lex attest`: cross-stage attestation queries, retro-blocking, import / push / pull.

use super::*;

/// `lex attest filter --kind K --result R --since T --store DIR`
/// — cross-stage attestation query (#132). Walks every primary
/// attestation file under `<store>/attestations/` and filters by
/// the supplied criteria. Designed for CI / dashboard queries
/// that span the whole log rather than a single stage.
pub(super) fn cmd_attest(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| {
        anyhow!(
            "usage: lex attest {{filter|import-install|push|pull|retro-block|retro-unblock}} ..."
        )
    })?;
    let rest = &args[1..];
    if sub == "push" {
        return cmd_attest_push(fmt, rest);
    }
    if sub == "pull" {
        return cmd_attest_pull(fmt, rest);
    }
    match sub.as_str() {
        "filter" => {
            let mut kind_filter: Option<String> = None;
            let mut result_filter: Option<String> = None;
            let mut since: Option<u64> = None;
            let mut store_root: Option<PathBuf> = None;
            // #246: `--run <id>` filters to Trace attestations whose
            // `kind.run_id` matches. Implies `--kind trace`.
            let mut run_filter: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--kind" => {
                        kind_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    "--result" => {
                        result_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    "--since" => {
                        let raw = rest
                            .get(i + 1)
                            .ok_or_else(|| anyhow!("--since needs a value"))?;
                        since = Some(parse_since(raw).ok_or_else(|| {
                            anyhow!("--since must be epoch seconds or YYYY-MM-DD, got `{raw}`")
                        })?);
                        i += 2;
                    }
                    "--store" => {
                        store_root = rest.get(i + 1).map(PathBuf::from);
                        i += 2;
                    }
                    "--run" => {
                        run_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    other => bail!("unexpected arg `{other}`"),
                }
            }
            let root = store_root.unwrap_or_else(default_store_root);
            let store = Store::open(&root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            let log = store.attestation_log()?;
            // `--run` uses the by-run secondary index instead of
            // walking every attestation; this is `O(traces of that
            // run)` rather than `O(all attestations)`.
            let all = match &run_filter {
                Some(rid) => log.list_for_run(rid)?,
                None => log.list_all()?,
            };

            let mut filtered: Vec<lex_vcs::Attestation> = all
                .into_iter()
                .filter(|a| {
                    if let Some(k) = &kind_filter {
                        if attestation_kind_tag(&a.kind) != *k {
                            return false;
                        }
                    }
                    if let Some(r) = &result_filter {
                        if attestation_result_tag(&a.result) != *r {
                            return false;
                        }
                    }
                    if let Some(s) = since {
                        if a.timestamp < s {
                            return false;
                        }
                    }
                    true
                })
                .collect();
            filtered.sort_by_key(|a| std::cmp::Reverse(a.timestamp));

            let data = serde_json::json!({
                "count": filtered.len(),
                "attestations": serde_json::to_value(&filtered)?,
            });
            let printable = filtered.clone();
            acli::emit_or_text("attest", data, fmt, move || {
                if printable.is_empty() {
                    println!("(no attestations match)");
                    return;
                }
                for a in &printable {
                    let kind = attestation_kind_tag(&a.kind);
                    let result = attestation_result_tag(&a.result);
                    println!(
                        "{}\t{}\t{}\t{:.16}…\tby={}@{}",
                        a.timestamp,
                        kind,
                        result,
                        a.stage_id,
                        a.produced_by.tool,
                        a.produced_by.version,
                    );
                }
            });
            Ok(())
        }
        "import-install" => cmd_attest_import_install(fmt, rest),
        "retro-block" => cmd_attest_retro_block(fmt, rest),
        "retro-unblock" => cmd_attest_retro_unblock(fmt, rest),
        other => bail!("unknown `lex attest` subcommand: {other}"),
    }
}

/// `lex attest import-install --audit <lex-os-audit.json> [--store DIR]`
/// (lex-os#36 / #38). Promotes lex-os capsule-install records into the
/// durable attestation graph so an install becomes queryable evidence —
/// not a throwaway audit file.
///
/// Reads a tamper-evident audit log (as written by `lex-os capsule
/// install --audit-out`) and, for every `capsule_installed` event,
/// emits a [`lex_vcs::AttestationKind::CapsuleInstall`] under
/// `stage_id == signer` with `produced_by.tool == signer`. That keys the
/// record by the publisher in both the by-stage index and the
/// producer-trust scorer, so `lex producer-trust recompute --tool
/// <signer>` folds real installs into the signer's earned trust and the
/// keyring `capsule install --trusted-keys` consumes.
///
/// Idempotent: the content-addressed attestation id dedups re-imports of
/// the same log (two installs of identical bytes by the same signer are
/// one fact). The audit log is *not* re-verified here — promotion records
/// what lex-os decided; verifying the chain remains `lex-os audit verify`.
pub(super) fn cmd_attest_import_install(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut audit_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--audit" => {
                audit_path = rest.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let audit_path = audit_path.ok_or_else(|| {
        anyhow!("usage: lex attest import-install --audit <lex-os-audit.json> [--store DIR]")
    })?;
    let raw = std::fs::read_to_string(&audit_path)
        .with_context(|| format!("reading audit log {}", audit_path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing audit log {} as JSON", audit_path.display()))?;
    let entries = parsed.as_array().ok_or_else(|| {
        anyhow!(
            "audit log {} is not a JSON array of entries",
            audit_path.display()
        )
    })?;

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;

    let mut imported: Vec<serde_json::Value> = Vec::new();
    let mut already_present = 0usize;
    for entry in entries {
        let event = &entry["event"];
        if event["kind"].as_str() != Some("capsule_installed") {
            continue;
        }
        // A malformed install event is a hard error: refuse to mint a
        // half-attributed trust record rather than silently skip it.
        let signer = event["signer"]
            .as_str()
            .ok_or_else(|| anyhow!("capsule_installed event missing `signer`"))?;
        let artifact = event["artifact"]
            .as_str()
            .ok_or_else(|| anyhow!("capsule_installed event missing `artifact`"))?;
        let effective_grant = event["effective_grant"].as_str().unwrap_or("").to_string();
        // content_hash names the exact bytes. Older logs predate it; we
        // import them with an empty hash rather than refusing.
        let content_hash = event["content_hash"].as_str().unwrap_or("").to_string();

        let producer = lex_vcs::ProducerDescriptor {
            tool: signer.to_string(),
            version: "lex-os-capsule".into(),
            model: None,
        };
        let attestation = lex_vcs::Attestation::new(
            signer.to_string(),
            None,
            None,
            lex_vcs::AttestationKind::CapsuleInstall {
                artifact: artifact.to_string(),
                content_hash: content_hash.clone(),
                signer: signer.to_string(),
                effective_grant,
            },
            lex_vcs::AttestationResult::Passed,
            producer,
            None,
        );
        let existed = log.get(&attestation.attestation_id)?.is_some();
        log.put(&attestation)?;
        if existed {
            already_present += 1;
        }
        imported.push(serde_json::json!({
            "attestation_id": attestation.attestation_id,
            "artifact": artifact,
            "signer": signer,
            "content_hash": content_hash,
            "already_present": existed,
        }));
    }

    let count = imported.len();
    let data = serde_json::json!({
        "audit_log": audit_path.display().to_string(),
        "imported": count,
        "already_present": already_present,
        "attestations": imported,
    });
    acli::emit_or_text("attest", data, fmt, move || {
        println!(
            "→ imported {count} capsule-install attestation(s) ({already_present} already present)"
        );
    });
    Ok(())
}

/// `lex attest retro-block --producer <tool_id> --reason "..."` (#248).
/// Emits an `AttestationKind::ProducerBlock` attestation under
/// `stage_id == tool_id`. The branch advance gate consults the
/// resulting record on every subsequent apply and refuses to
/// advance over an op whose stage carries an attestation produced
/// by `tool_id` at or after this block's timestamp.
pub(super) fn cmd_attest_retro_block(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut producer: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--producer" => {
                producer = rest.get(i + 1).cloned();
                i += 2;
            }
            "--reason" => {
                reason = rest.get(i + 1).cloned();
                i += 2;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool_id = producer.ok_or_else(|| {
        anyhow!("usage: lex attest retro-block --producer <tool_id> --reason \"...\"")
    })?;
    let reason = reason.ok_or_else(|| anyhow!("lex attest retro-block: --reason required"))?;

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let attestation = lex_vcs::Attestation::with_timestamp(
        tool_id.clone(),
        None,
        None,
        lex_vcs::AttestationKind::ProducerBlock {
            tool_id: tool_id.clone(),
            reason: reason.clone(),
            blocked_at: now,
        },
        // The verdict on the *block itself* is always Passed —
        // it's a declaration, not the result of a verification.
        // Failure to land the attestation surfaces as an io error,
        // not as `Failed { detail }`.
        lex_vcs::AttestationResult::Passed,
        retro_block_producer(),
        None,
        now,
    );
    log.put(&attestation)?;
    // #256: a fresh ProducerBlock invalidates every branch's
    // walk-back gate checkpoint, forcing the next advance to
    // re-walk the chain and discover any contamination.
    let invalidated = store
        .invalidate_gate_checkpoints()
        .with_context(|| "invalidating gate checkpoints after retro-block")?;

    let data = serde_json::json!({
        "tool_id": &tool_id,
        "reason": &reason,
        "blocked_at": now,
        "attestation_id": &attestation.attestation_id,
        "branches_invalidated": invalidated,
    });
    let printable_tool = tool_id.clone();
    acli::emit_or_text("attest", data, fmt, move || {
        println!("→ retroactively blocked producer `{printable_tool}` at {now}");
    });
    Ok(())
}

/// `lex attest retro-unblock --producer <tool_id> --reason "..."` (#248).
/// Counterpart to `retro-block`. Emits an
/// `AttestationKind::ProducerUnblock` so the gate honors the most
/// recent verdict per `tool_id` by timestamp.
pub(super) fn cmd_attest_retro_unblock(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut producer: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--producer" => {
                producer = rest.get(i + 1).cloned();
                i += 2;
            }
            "--reason" => {
                reason = rest.get(i + 1).cloned();
                i += 2;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool_id = producer.ok_or_else(|| {
        anyhow!("usage: lex attest retro-unblock --producer <tool_id> --reason \"...\"")
    })?;
    let reason = reason.ok_or_else(|| anyhow!("lex attest retro-unblock: --reason required"))?;

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let attestation = lex_vcs::Attestation::with_timestamp(
        tool_id.clone(),
        None,
        None,
        lex_vcs::AttestationKind::ProducerUnblock {
            tool_id: tool_id.clone(),
            reason: reason.clone(),
            unblocked_at: now,
        },
        lex_vcs::AttestationResult::Passed,
        retro_block_producer(),
        None,
        now,
    );
    log.put(&attestation)?;
    // #256: an unblock can also unblock previously-refused branch
    // advances. Invalidate so the next advance re-walks and lets
    // through anything the unblock cleared.
    let invalidated = store
        .invalidate_gate_checkpoints()
        .with_context(|| "invalidating gate checkpoints after retro-unblock")?;

    let data = serde_json::json!({
        "tool_id": &tool_id,
        "reason": &reason,
        "unblocked_at": now,
        "attestation_id": &attestation.attestation_id,
        "branches_invalidated": invalidated,
    });
    let printable_tool = tool_id.clone();
    acli::emit_or_text("attest", data, fmt, move || {
        println!("→ retroactively unblocked producer `{printable_tool}` at {now}");
    });
    Ok(())
}

/// Producer descriptor for the synthetic `ProducerBlock` /
/// `ProducerUnblock` attestations written by the `lex attest
/// retro-{block,unblock}` commands. Tagged distinctly from
/// `lex run --trace`'s `trace_producer` so the activity feed can
/// tell the two apart.
pub(super) fn retro_block_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex attest retro-block".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// `lex attest push <remote_url> [--since-op OP_ID] [--store DIR]
/// [--dry-run]` (#242).
///
/// Walks the local attestation log, optionally filtering to
/// attestations whose `op_id` is `>= --since-op` (in DAG order, not
/// timestamp), and posts them to `<remote_url>/v1/attestations/batch`.
///
/// Without `--since-op`, sends every attestation. The server-side
/// idempotency check (content-addressed `attestation_id`) makes
/// "push everything" safe; `--since-op` is purely an optimization
/// for large logs.
///
/// Idempotency: re-pushing the same attestations converges to
/// `added: 0`. Network failure mid-push leaves the remote with the
/// prefix that landed; re-running picks up where the failure
/// occurred.
pub(super) fn cmd_attest_push(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut remote: Option<String> = None;
    let mut since_op: Option<String> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since-op" => {
                since_op = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--since-op needs an op_id"))?
                        .clone(),
                );
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| {
        anyhow!("usage: lex attest push <remote_url> [--since-op OP_ID] [--dry-run] [--store DIR]")
    })?;

    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;

    // Filter by op-id ancestry when --since-op is set: only push
    // attestations whose op_id is reachable from the local op log
    // and not in the ancestry of `since_op`. Without --since-op,
    // send every attestation.
    let all = log.list_all()?;
    let to_send: Vec<lex_vcs::Attestation> = match since_op.as_ref() {
        None => all,
        Some(cutoff) => {
            let op_log = lex_vcs::OpLog::open(&root)?;
            // Set of op_ids we should NOT re-send: every ancestor of
            // `cutoff`, inclusive.
            let exclude: std::collections::BTreeSet<String> = op_log
                .walk_back(cutoff, None)?
                .into_iter()
                .map(|r| r.op_id)
                .collect();
            all.into_iter()
                .filter(|a| match &a.op_id {
                    Some(op_id) => !exclude.contains(op_id),
                    None => true,
                })
                .collect()
        }
    };

    if dry_run {
        let ids: Vec<&String> = to_send.iter().map(|a| &a.attestation_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "since_op": since_op,
            "would_send": to_send.len(),
            "attestation_ids": ids,
        });
        let count = to_send.len();
        let remote_text = remote.clone();
        acli::emit_or_text("attest-push", data, fmt, move || {
            println!("would push {count} attestations to {remote_text} (dry-run)");
        });
        return Ok(());
    }

    if to_send.is_empty() {
        let data = serde_json::json!({
            "remote": remote,
            "received": 0,
            "added": 0,
            "skipped": 0,
        });
        acli::emit_or_text("attest-push", data, fmt, || {
            println!("nothing to push (no attestations match)");
        });
        return Ok(());
    }

    let url = format!("{}/v1/attestations/batch", remote.trim_end_matches('/'));
    let body = serde_json::to_string(&to_send).map_err(|e| anyhow!("serializing batch: {e}"))?;
    let resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status().as_u16();
    let resp_body: serde_json::Value = resp
        .into_body()
        .read_json()
        .map_err(|e| anyhow!("decoding response: {e}"))?;
    if status >= 400 {
        bail!("server rejected batch (HTTP {status}): {resp_body}");
    }

    let received = resp_body
        .get("received")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let added = resp_body.get("added").and_then(|v| v.as_u64()).unwrap_or(0);
    let skipped = resp_body
        .get("skipped")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let data = serde_json::json!({
        "remote": remote,
        "received": received,
        "added": added,
        "skipped": skipped,
    });
    let remote_text = remote.clone();
    acli::emit_or_text("attest-push", data, fmt, move || {
        println!(
            "pushed {received} attestations to {remote_text}: \
             {added} added, {skipped} skipped (already present)"
        );
    });
    Ok(())
}

/// `lex attest pull <remote_url> [--since-op OP_ID] [--limit N]
/// [--dry-run] [--store DIR]` (#260).
///
/// Append-only fetch of attestations — the inverse of `lex attest
/// push`. Asks the remote for attestations whose `op_id` is not in
/// the ancestry of `--since-op` (or, without the flag, whose
/// `op_id` we don't already know about), validates each, and
/// persists.
///
/// Idempotency: re-running converges to `added: 0`. Network failure
/// mid-pull leaves the local with the prefix that landed; the next
/// run picks up where the failure occurred.
pub(super) fn cmd_attest_pull(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut remote: Option<String> = None;
    let mut since_op: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since-op" => {
                since_op = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--since-op needs an op_id"))?
                        .clone(),
                );
            }
            "--limit" => {
                limit = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--limit needs N"))?
                        .parse()
                        .map_err(|e| anyhow!("--limit: {e}"))?,
                );
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| anyhow!(
        "usage: lex attest pull <remote_url> [--since-op OP_ID] [--limit N] [--dry-run] [--store DIR]"
    ))?;

    let mut url = format!("{}/v1/attestations/since", remote.trim_end_matches('/'),);
    let mut sep = '?';
    if let Some(op) = &since_op {
        url.push_str(&format!("{sep}after-op={op}"));
        sep = '&';
    }
    if let Some(n) = limit {
        url.push_str(&format!("{sep}limit={n}"));
    }
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp
            .into_body()
            .read_to_string()
            .unwrap_or_else(|_| "(unreadable body)".into());
        bail!("server returned HTTP {status}: {body}");
    }
    let received: Vec<lex_vcs::Attestation> = resp
        .into_body()
        .read_json()
        .map_err(|e| anyhow!("decoding response from {url}: {e}"))?;

    if dry_run {
        let ids: Vec<&String> = received.iter().map(|a| &a.attestation_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "since_op": since_op,
            "would_receive": received.len(),
            "attestation_ids": ids,
        });
        let count = received.len();
        let remote_text = remote.clone();
        acli::emit_or_text("attest-pull", data, fmt, move || {
            println!("would pull {count} attestations from {remote_text} (dry-run)");
        });
        return Ok(());
    }

    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let op_log = lex_vcs::OpLog::open(&root)?;

    let mut added = 0usize;
    let mut rejected_unknown_op = 0usize;
    for att in &received {
        // Validate content-addressing.
        let expected = lex_vcs::Attestation::with_timestamp(
            att.stage_id.clone(),
            att.op_id.clone(),
            att.intent_id.clone(),
            att.kind.clone(),
            att.result.clone(),
            att.produced_by.clone(),
            att.cost.clone(),
            att.timestamp,
        )
        .attestation_id;
        if expected != att.attestation_id {
            bail!(
                "remote returned attestation with mismatched id: supplied={}, expected={}",
                att.attestation_id,
                expected,
            );
        }
        // If the attestation references an op_id, that op must
        // already exist locally — otherwise the attestation is
        // dangling. Skip rather than fail the whole pull; the
        // caller can re-issue after pulling the missing ops.
        if let Some(op_id) = &att.op_id {
            if op_log.get(op_id)?.is_none() {
                rejected_unknown_op += 1;
                continue;
            }
        }
        let was_present = log.get(&att.attestation_id)?.is_some();
        log.put(att)?;
        if !was_present {
            added += 1;
        }
    }

    let data = serde_json::json!({
        "remote": remote,
        "received": received.len(),
        "added": added,
        "skipped": received.len() - added - rejected_unknown_op,
        "rejected_unknown_op": rejected_unknown_op,
    });
    let total = received.len();
    let skipped = received.len() - added - rejected_unknown_op;
    let remote_text = remote.clone();
    acli::emit_or_text("attest-pull", data, fmt, move || {
        println!(
            "pulled {total} attestations from {remote_text}: \
             {added} new, {skipped} already present, {rejected_unknown_op} skipped (unknown op_id)"
        );
    });
    Ok(())
}

pub(super) fn attestation_kind_tag(k: &lex_vcs::AttestationKind) -> &'static str {
    use lex_vcs::AttestationKind::*;
    match k {
        Examples { .. } => "examples",
        Spec { .. } => "spec",
        DiffBody { .. } => "diff_body",
        TypeCheck => "type_check",
        EffectAudit => "effect_audit",
        SandboxRun { .. } => "sandbox_run",
        Override { .. } => "override",
        Defer { .. } => "defer",
        Block { .. } => "block",
        Unblock { .. } => "unblock",
        Trace { .. } => "trace",
        ProducerBlock { .. } => "producer_block",
        ProducerUnblock { .. } => "producer_unblock",
        RepairHint { .. } => "repair_hint",
        RepairAttempt { .. } => "repair_attempt",
        ProducerTrust { .. } => "producer_trust",
        TrustWaived { .. } => "trust_waived",
        CapsuleInstall { .. } => "capsule_install",
    }
}
