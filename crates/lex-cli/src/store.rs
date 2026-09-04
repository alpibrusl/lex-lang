//! `lex publish` and `lex store *`: publishing stages into the content store, store maintenance and search.

use super::*;

/// Build the embedder used by `lex store search` / `lex audit
/// --query`. When `LEX_EMBED_URL` is set we wire up an HTTP backend
/// (Ollama or OpenAI-compat per `LEX_EMBED_PROVIDER`) wrapped in a
/// filesystem cache under `<store>/search/embeddings/`. Otherwise
/// we use the deterministic [`lex_search::MockEmbedder`].
pub(crate) fn build_embedder(
    store_root: &std::path::Path,
) -> Result<Box<dyn lex_search::Embedder>> {
    if let Some(http) = lex_search::HttpEmbedder::from_env()
        .map_err(|e| anyhow!("LEX_EMBED_URL configuration: {e}"))?
    {
        let fingerprint = format!("{:?}:{}", http.provider(), http.model());
        let cache_root = lex_search::default_cache_root(store_root);
        let cached = lex_search::CachingEmbedder::new(http, cache_root, fingerprint);
        Ok(Box::new(cached))
    } else {
        Ok(Box::new(lex_search::MockEmbedder::new()))
    }
}

pub(super) fn cmd_publish(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    use lex_vcs::ImportMap;

    let (root, rest, activate, dry_run) = parse_store_flag(args);
    // Pull --branch and --signing-key off as well.
    let mut branch: Option<String> = None;
    let mut signing_key_flag: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--branch needs a value"))?
                    .clone(),
            );
        } else if a == "--signing-key" {
            signing_key_flag = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--signing-key needs a hex value"))?
                    .clone(),
            );
        } else {
            positional.push(a.clone());
        }
    }
    let path = positional.first().ok_or_else(|| {
        anyhow!(
        "usage: lex publish [--store DIR] [--branch NAME] [--activate] [--signing-key HEX] <file>")
    })?;
    let signer = resolve_signing_key(signing_key_flag.as_deref())?;

    let prog = read_program(path)?;
    // #168: type-check *and* rewrite stdlib parse calls so a
    // typed `toml.parse[T]` validates required fields before
    // returning Ok. The mutation lands in the canonical AST so
    // every downstream consumer (bytecode compile, store
    // publish) sees the strict shape.
    let mut stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        let arr: Vec<serde_json::Value> = errs
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("publish", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(2);
    }

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // Compute the diff. We need the old fns and new fns.
    let old_head = store.branch_head(&branch)?;
    let old_fns: BTreeMap<String, lex_ast::FnDecl> = old_head
        .values()
        .filter_map(|stg| store.get_ast(stg).ok())
        .filter_map(|s| match s {
            Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect();
    let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|s| match s {
            Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let report = diff::compute_diff(&old_fns, &new_fns, /* body_patches: */ true);

    // Build new imports map (one entry per source file we just read).
    let mut new_imports: ImportMap = ImportMap::new();
    // Stable, transport-independent key. Per-file imports are not
    // currently tracked separately — all imports of one publish are
    // grouped under "<source>" so that publishing the same source
    // via CLI vs HTTP produces identical op_ids.
    let file_key = "<source>".to_string();
    let entry = new_imports.entry(file_key).or_default();
    for s in &stages {
        if let Stage::Import(im) = s {
            entry.insert(im.reference.clone());
        }
    }

    if dry_run {
        // Compute the op kinds for the dry-run preview using diff_to_ops
        // directly, without persisting anything.
        let old_name_to_sig: BTreeMap<String, String> = old_head
            .iter()
            .filter_map(|(sig, stg)| store.get_metadata(stg).ok().map(|m| (m.name, sig.clone())))
            .collect();
        let old_effects: BTreeMap<String, BTreeSet<String>> = old_head
            .iter()
            .filter_map(|(sig, stg)| {
                let ast = store.get_ast(stg).ok()?;
                match ast {
                    Stage::FnDecl(fd) => {
                        let s: BTreeSet<String> =
                            fd.effects.iter().map(|e| e.name.clone()).collect();
                        Some((sig.clone(), s))
                    }
                    _ => None,
                }
            })
            .collect();
        let old_imports = store.derive_imports_from_oplog(&branch)?;
        let op_kinds = lex_vcs::diff_to_ops(lex_vcs::DiffInputs {
            old_head: &old_head,
            old_name_to_sig: &old_name_to_sig,
            old_effects: &old_effects,
            old_imports: &old_imports,
            new_stages: &stages,
            new_imports: &new_imports,
            diff: &report,
        })
        .map_err(|e| anyhow!("diff_to_ops: {e}"))?;
        let actions: Vec<serde_json::Value> = op_kinds
            .iter()
            .map(|k| serde_json::to_value(k).unwrap())
            .collect();
        acli::emit_dry_run(
            "publish",
            fmt,
            &format!("would apply {} op(s) to branch {}", op_kinds.len(), branch),
            actions,
        );
        return Ok(());
    }

    let outcome = store.publish_program_signed(
        &branch,
        &stages,
        &report,
        &new_imports,
        activate,
        signer.as_ref(),
    )?;
    let signed = signer.as_ref().map(|kp| kp.public_hex());
    let data = serde_json::json!({
        "ops": outcome.ops,
        "head_op": outcome.head_op,
        "signed_by": signed,
    });
    acli::emit_or_text("publish", data, fmt, || {});
    Ok(())
}

pub(super) fn cmd_store(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex store {{list|get}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "list" => {
            let (root, _rest, _, _) = parse_store_flag(rest);
            let store = Store::open(&root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            let sigs = store.list_sigs()?;
            let entries: Vec<serde_json::Value> = sigs
                .iter()
                .map(|s| {
                    let active = store.resolve_sig(s).ok().flatten().unwrap_or_default();
                    serde_json::json!({ "sig_id": s, "active_stage_id": active })
                })
                .collect();
            let data = serde_json::json!({ "sigs": entries });
            acli::emit_or_text("store", data, fmt, || {
                for s in &sigs {
                    let active = store.resolve_sig(s).ok().flatten().unwrap_or_default();
                    println!("{s}\tactive={active}");
                }
            });
            Ok(())
        }
        "get" => {
            let (root, rest, _, _) = parse_store_flag(rest);
            let store = Store::open(&root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            // #227 verification flags. `--require-signed` rejects an
            // unsigned stage; `--trusted-key HEX` rejects any stage
            // whose signature was made by a different key. Both are
            // independent: `--trusted-key` implies signed.
            let mut require_signed = false;
            let mut trusted_key: Option<String> = None;
            let mut positional: Vec<&String> = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--require-signed" {
                    require_signed = true;
                } else if a == "--trusted-key" {
                    trusted_key = Some(
                        it.next()
                            .ok_or_else(|| anyhow!("--trusted-key needs a hex value"))?
                            .clone(),
                    );
                    require_signed = true;
                } else {
                    positional.push(a);
                }
            }
            let id = positional.first().ok_or_else(|| {
                anyhow!("usage: lex store get [--require-signed] [--trusted-key HEX] <stage_id>")
            })?;
            let meta = store.get_metadata(id)?;
            verify_metadata_signature(&meta, require_signed, trusted_key.as_deref())?;
            let ast = store.get_ast(id)?;
            let v = serde_json::json!({
                "metadata": serde_json::to_value(&meta)?,
                "status": format!("{:?}", store.get_status(id)?).to_lowercase(),
                "ast": serde_json::to_value(&ast)?,
                "signature_verified": meta.signature.is_some(),
            });
            acli::emit_or_text("store", v.clone(), fmt, || {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            });
            Ok(())
        }
        "search" => cmd_store_search(fmt, rest),
        "migrate-ops" => cmd_store_migrate_ops(fmt, rest),
        other => bail!("unknown `lex store` subcommand: {other}"),
    }
}

/// `lex store migrate-ops` (#244). Re-canonicalize every op in the
/// store under a target [`OperationFormat`]. Today only V1 exists,
/// so the production migration is always a no-op; the command
/// surfaces the plan/apply mechanism that future format bumps will
/// rely on.
///
/// Flags:
/// * `--to v1` (required) — the target format. Future variants will
///   accept their own tags.
/// * `--dry-run` — print the old→new mapping without rewriting any
///   files. Mutually exclusive with `--confirm`.
/// * `--confirm` — apply the migration. **Destructive**: deletes
///   the old `<root>/ops/<old_op_id>.json` files and rewrites
///   `<root>/branches/*.json` so `head_op` references the new ids.
///   Attestations are *not* rewritten in this slice — see #244 and
///   the attestation cascade follow-up.
pub(super) fn cmd_store_migrate_ops(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // `parse_store_flag` already consumes `--dry-run` and returns it
    // as the 4th tuple element; we honor that, not a re-parse from
    // the remainder.
    let (root, rest, _activate, dry_run) = parse_store_flag(args);
    let mut target_str: Option<String> = None;
    let mut confirm = false;
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--to" => {
                target_str = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--to needs a format tag (today: v1)"))?
                        .clone(),
                );
            }
            "--confirm" => confirm = true,
            other => bail!("unknown flag `{other}` for `lex store migrate-ops`"),
        }
    }
    if dry_run && confirm {
        bail!("--dry-run and --confirm are mutually exclusive");
    }
    if !dry_run && !confirm {
        bail!(
            "lex store migrate-ops is destructive — pass --dry-run to preview, \
             --confirm to apply"
        );
    }
    let target_str = target_str.ok_or_else(|| anyhow!("--to <format> is required (today: v1)"))?;
    let target: lex_vcs::OperationFormat = match target_str.as_str() {
        "v1" | "V1" => lex_vcs::OperationFormat::V1,
        other => bail!("unknown operation format `{other}` — supported: v1"),
    };

    let log = lex_vcs::OpLog::open(&root)
        .with_context(|| format!("opening op log at {}", root.display()))?;
    let plan =
        lex_vcs::migrate::plan_migration(&log, target).with_context(|| "planning migration")?;

    let mapping = plan.mapping();
    let changed: Vec<&lex_vcs::migrate::MigrationStep> = plan
        .steps
        .iter()
        .filter(|s| s.old_op_id != s.new_op_id)
        .collect();

    let mappings_json: Vec<serde_json::Value> = plan
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "old": s.old_op_id,
                "new": s.new_op_id,
                "changed": s.old_op_id != s.new_op_id,
            })
        })
        .collect();

    let summary = serde_json::json!({
        "target_format": format!("{:?}", target).to_lowercase(),
        "total_ops": plan.steps.len(),
        "rotated_op_ids": changed.len(),
        "is_no_op": plan.is_no_op(),
        "applied": false,
        "mappings": mappings_json,
    });

    if dry_run {
        acli::emit_or_text("store-migrate-ops", summary.clone(), fmt, || {
            println!(
                "would migrate {} ops to {:?}; {} op_ids would rotate (dry-run, no files written)",
                plan.steps.len(),
                target,
                changed.len(),
            );
            for s in &plan.steps {
                if s.old_op_id != s.new_op_id {
                    println!("  {} → {}", s.old_op_id, s.new_op_id);
                }
            }
            if !changed.is_empty() {
                println!(
                    "\nNote: applying with --confirm will also rewrite branch heads \
                     and cascade-migrate attestations whose `op_id` rotated (#258)."
                );
            }
        });
        return Ok(());
    }

    // --confirm path: apply.
    lex_vcs::migrate::apply_migration(&log, &plan).with_context(|| "applying op-log migration")?;

    let branch_updates =
        rewrite_branch_heads(&root, &mapping).with_context(|| "rewriting branch heads")?;

    // #258: cascade migrate attestations whose `op_id` references
    // a rotated op. Their `attestation_id` is computed including
    // op_id, so they all rotate too.
    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let attest_log = store
        .attestation_log()
        .with_context(|| "opening attestation log")?;
    let att_steps = lex_vcs::migrate::plan_attestation_migration(&attest_log, &mapping)
        .with_context(|| "planning attestation cascade")?;
    lex_vcs::migrate::apply_attestation_migration(&attest_log, &att_steps)
        .with_context(|| "applying attestation cascade")?;
    let attestations_rotated = att_steps.iter().filter(|s| !s.is_no_op()).count();

    // Invalidate the gate-checkpoint pointers on every branch
    // (#256). They reference op_ids by content, which the
    // migration just rotated; without invalidation the next
    // advance would compare against a stale id and re-walk
    // unnecessarily (or, worse, treat the new head as "already
    // verified" because its old name happened to match).
    let _ = store.invalidate_gate_checkpoints();

    let summary = serde_json::json!({
        "target_format": format!("{:?}", target).to_lowercase(),
        "total_ops": plan.steps.len(),
        "rotated_op_ids": changed.len(),
        "is_no_op": plan.is_no_op(),
        "applied": true,
        "branches_updated": branch_updates,
        "attestations_rotated": attestations_rotated,
        "mappings": summary["mappings"].clone(),
    });
    acli::emit_or_text("store-migrate-ops", summary, fmt, || {
        println!(
            "migrated {} ops to {:?}; {} op_ids rotated; \
             {} branch heads rewritten; {} attestations cascade-migrated",
            plan.steps.len(),
            target,
            changed.len(),
            branch_updates,
            attestations_rotated,
        );
    });
    Ok(())
}

/// Walk `<root>/branches/*.json`, parse each, and rewrite `head_op`
/// in place if the current value appears in `mapping`. Returns the
/// number of branch files that changed.
///
/// Bypasses `lex-store`'s `set_branch_head_op` (which is `pub(crate)`)
/// because this is a one-shot supervised rewrite invoked by the
/// `migrate-ops` command — not a normal write path.
pub(super) fn rewrite_branch_heads(
    root: &std::path::Path,
    mapping: &std::collections::BTreeMap<String, String>,
) -> Result<usize> {
    let dir = root.join("branches");
    if !dir.exists() {
        return Ok(0);
    }
    let mut updated = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut changed = false;
        if let Some(head) = value.get("head_op").and_then(|v| v.as_str()) {
            if let Some(new) = mapping.get(head) {
                value["head_op"] = serde_json::Value::String(new.clone());
                changed = true;
            }
        }
        if changed {
            let new_bytes = serde_json::to_vec_pretty(&value)
                .with_context(|| format!("serializing {}", path.display()))?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &new_bytes)?;
            std::fs::rename(&tmp, &path)?;
            updated += 1;
        }
    }
    Ok(updated)
}

/// `lex store search "<query>"` (#224). Embeds the query and ranks
/// every active stage in the store by fused cosine similarity over
/// description + signature + examples. Slice 1 ships only the
/// MockEmbedder for offline / deterministic ranking; the network-
/// backed providers gate on `LEX_EMBED_URL` (slice 2).
pub(super) fn cmd_store_search(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // `lex store search reindex` warms the embedding cache by
    // walking every active stage through `SearchIndex::build`
    // (#283). Falls through to query mode for any non-reindex
    // positional.
    if matches!(args.first().map(String::as_str), Some("reindex")) {
        return cmd_store_search_reindex(fmt, &args[1..]);
    }
    let (root, rest, _, _) = parse_store_flag(args);
    let mut limit: usize = 10;
    let mut query: Option<String> = None;
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--limit" => {
                let v = iter
                    .next()
                    .ok_or_else(|| anyhow!("--limit needs a value"))?;
                limit = v.parse().context("--limit must be a positive integer")?;
            }
            other if !other.starts_with("--") => {
                if query.is_some() {
                    bail!("usage: lex store search [--limit N] \"<query>\"");
                }
                query = Some(other.to_string());
            }
            other => bail!("unknown flag `{other}` for `lex store search`"),
        }
    }
    let query = query.ok_or_else(|| anyhow!("usage: lex store search [--limit N] \"<query>\""))?;

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = build_embedder(&root)?;
    let idx = lex_search::SearchIndex::build(&store, &*embedder)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let hits = idx
        .query(&*embedder, &query, limit)
        .map_err(|e| anyhow!("query embedding: {e}"))?;
    let v = serde_json::json!({
        "query": &query,
        "limit": limit,
        "indexed": idx.stages.len(),
        "hits": serde_json::to_value(&hits)?,
    });
    acli::emit_or_text("store-search", v.clone(), fmt, || {
        println!("{} hit(s) for `{}`", hits.len(), query);
        for h in &hits {
            println!(
                "  {:>6.3}  {}::{}  {}",
                h.score.fused, h.stage_id, h.name, h.signature,
            );
            if let Some(d) = &h.description {
                println!("          note: {d}");
            }
        }
    });
    Ok(())
}

/// `lex store search reindex [--store DIR]` (#283). Walks every
/// active stage through the configured embedder, populating the
/// on-disk cache so subsequent `lex store search <query>` calls
/// don't pay the embedding cost on the cold path.
///
/// With `LEX_EMBED_URL` set, this calls the HTTP backend (Ollama or
/// OpenAI-compat per `LEX_EMBED_PROVIDER`); without it, falls back
/// to [`lex_search::MockEmbedder`] (fast but semantically random —
/// useful for warming a deterministic test fixture).
///
/// Emits `{ indexed, dim, embedder, store }` as the JSON envelope.
pub(super) fn cmd_store_search_reindex(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, _rest, _, _) = parse_store_flag(args);
    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = build_embedder(&root)?;
    let started = std::time::Instant::now();
    let idx = lex_search::SearchIndex::build(&store, &*embedder)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let v = serde_json::json!({
        "indexed": idx.stages.len(),
        "dim": embedder.dim(),
        "elapsed_ms": elapsed_ms,
        "store": root.display().to_string(),
    });
    acli::emit_or_text("store-search-reindex", v.clone(), fmt, || {
        println!(
            "indexed {} stage(s) ({}-dim embeddings, {} ms)",
            idx.stages.len(),
            embedder.dim(),
            elapsed_ms
        );
    });
    Ok(())
}
