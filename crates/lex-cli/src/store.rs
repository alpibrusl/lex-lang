//! `lex publish` and `lex store *`: the publish CLI wrapper (the pipeline itself
//! lives in `publish_core.rs`), store maintenance and search.

use super::*;
use lex_syntax::{load_package, Manifest};

/// Read the source `lex publish` was given. A **directory** is a whole
/// package: every `src/**/*.lex` is loaded as one prefix-mangled program
/// (`<stem>_<hash>.<name>`), so two modules can each declare a `validate`
/// without colliding in the branch's name-keyed type-check scope
/// (#828/#894) — publishing a multi-module package module-by-module can't
/// (they collide). Returns the per-file import map for a package; a
/// single **file** is loaded as before and returns `None` (the caller
/// derives imports from the parsed `Import` stages).
pub(crate) fn read_publish_source(
    path: &str,
    inline_packages: bool,
) -> Result<(SynProgram, Option<lex_vcs::ImportMap>, BTreeMap<String, String>)> {
    read_publish_source_opt(path, inline_packages, /*allow_empty=*/ false)
}

/// [`read_publish_source`], optionally accepting a package with no `.lex`
/// sources as the **empty program** (#892 PR 1: an importer replaying a commit
/// that deletes the package must be able to publish "nothing" so `diff_to_ops`
/// emits the removals). Only an *absent* program is allowed: a package whose
/// `src/` is missing or holds no `.lex` file. Sources that exist still need a
/// valid `lex.toml`, exactly as before — `allow_empty` never papers over a
/// half-present package. `lex publish` passes `false`, so running it in an
/// empty package by mistake is still an error.
pub(crate) fn read_publish_source_opt(
    path: &str,
    inline_packages: bool,
    allow_empty: bool,
) -> Result<(SynProgram, Option<lex_vcs::ImportMap>, BTreeMap<String, String>)> {
    let p = std::path::Path::new(path);
    if !p.is_dir() {
        return Ok((read_program(path)?, None, BTreeMap::new()));
    }
    if allow_empty {
        let mut found: Vec<PathBuf> = Vec::new();
        collect_lex_files(&p.join("src"), &mut found);
        if found.is_empty() {
            let empty = SynProgram {
                items: Vec::new(),
                leading_comments: Vec::new(),
                trailing_comments: Vec::new(),
            };
            return Ok((empty, Some(lex_vcs::ImportMap::new()), BTreeMap::new()));
        }
    }
    let manifest = Manifest::load(&p.join("lex.toml"))
        .map_err(|e| anyhow!("reading {path}/lex.toml (a package publish needs it): {e}"))?;
    // The package name is mixed into every mangling key so two packages
    // with the same internal layout don't collapse onto one set of names.
    let namespace = manifest
        .package
        .as_ref()
        .map(|m| m.name.clone())
        .ok_or_else(|| anyhow!("{path}/lex.toml needs a [package] name to publish a package"))?;
    let src_dir = p.join("src");
    if !src_dir.is_dir() {
        bail!("package {path} has no src/ directory to publish");
    }
    let mut entries: Vec<PathBuf> = Vec::new();
    collect_lex_files(&src_dir, &mut entries);
    if entries.is_empty() {
        bail!("no .lex files under {path}/src");
    }
    // #930: the op-log publish loads without inlining registry/git
    // dependencies, so history keeps the `import` edges (the consumer's gate
    // resolves them) and refs stay `<alias>.name`; local (`./`) modules always
    // inline. The example gate re-loads WITH inlining, because running examples
    // needs the dependencies' implementations, not just their signatures.
    let loaded = load_package(&entries, p, &namespace, inline_packages)
        .map_err(|e| anyhow!("loading package {path}: {e}"))?;
    // `imports_by_file` now carries each import's real `as` alias, so a
    // non-default `import "lex-nt/lib" as nt` round-trips as `nt` (#909/#930).
    let mut imports = lex_vcs::ImportMap::new();
    for (file, modules) in &loaded.imports_by_file {
        let entry = imports.entry(file.clone()).or_default();
        for (reference, alias) in modules {
            entry.insert(lex_vcs::ImportRef {
                reference: reference.clone(),
                alias: alias.clone(),
            });
        }
    }
    Ok((loaded.program, Some(imports), loaded.module_prefixes))
}

/// Recursively collect `*.lex` files under `dir`, sorted for a
/// deterministic load order.
fn collect_lex_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut es: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    es.sort_by_key(|e| e.path());
    for e in es {
        let path = e.path();
        if path.is_dir() {
            collect_lex_files(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("lex") {
            out.push(path);
        }
    }
}

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
    let (root, rest, activate, dry_run) = parse_store_flag(args);
    // Pull --branch and --signing-key off as well.
    let mut branch: Option<String> = None;
    let mut signing_key_flag: Option<String> = None;
    // #131 / #839: an optional Intent to stamp onto every op this
    // publish emits (see `--intent-prompt`).
    let mut intent_prompt: Option<String> = None;
    let mut intent_model: Option<String> = None;
    let mut intent_session: Option<String> = None;
    // #949 phase 5: the typed issue this publish realizes. Stamped into the
    // Intent (`Intent.issue_id`), so the ops link back to the work item and
    // the derived issue state can see "work has started" from provenance.
    let mut intent_issue: Option<String> = None;
    // #1007 PR 4: a directory publish captures the working copy's non-op-log
    // files (README, lex.toml, lex.lock, tests/, ...) into a `SetFiles` op by
    // default. `--no-files` opts a single publish out; a single-**file**
    // publish never captures files regardless of this flag.
    let mut no_files = false;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--no-files" {
            no_files = true;
        } else if a == "--branch" {
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
        } else if a == "--intent-prompt" {
            intent_prompt = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--intent-prompt needs a value"))?
                    .clone(),
            );
        } else if a == "--intent-model" {
            intent_model = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--intent-model needs a provider/name value"))?
                    .clone(),
            );
        } else if a == "--intent-session" {
            intent_session = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--intent-session needs a value"))?
                    .clone(),
            );
        } else if a == "--intent-issue" {
            intent_issue = Some(
                it.next()
                    .ok_or_else(|| anyhow!("--intent-issue needs an issue id"))?
                    .clone(),
            );
        } else {
            positional.push(a.clone());
        }
    }
    // #970: `--intent-model` / `--intent-session` / `--intent-issue` no longer
    // require `--intent-prompt`. Every publish now records an intent (a
    // synthesized, explicitly-unattributed one when no prompt is given), so
    // these flags are meaningful on their own — binding an op to an issue or a
    // session without prose is useful, and refusing it only pushed callers
    // toward recording nothing at all.
    let path = positional.first().ok_or_else(|| {
        anyhow!(
        "usage: lex publish [--store DIR] [--branch NAME] [--activate] [--signing-key HEX] \
         [--intent-prompt TEXT] [--intent-model PROVIDER/NAME] [--intent-session ID] \
         [--intent-issue ISSUE_ID] [--no-files] <file|dir>\n\
         \n\
         Every publish records an Intent. Without --intent-prompt it is recorded \
         as explicitly unattributed (#970) — pass --intent-prompt to say why the \
         change was made, which is what makes `lex recall` and `lex op replay` useful.\n\
         \n\
         A directory publish also captures the working copy's non-op-log files \
         (README, lex.toml, lex.lock, tests/, ...) as one SetFiles op, last, under \
         the same intent (#1007) — pass --no-files to opt a single publish out.")
    })?;
    let signer = resolve_signing_key(signing_key_flag.as_deref())?;

    // Everything from here to the printed result is the shared publish core
    // (#892 PR 1); this wrapper only builds its inputs and renders its outcome.
    let intent = build_intent(intent_prompt, intent_model, intent_session, intent_issue);
    let mut opts = crate::publish_core::PublishOptions::new(intent);
    opts.activate = activate;
    opts.files = !no_files;
    opts.signer = signer.as_ref();
    opts.dry_run = dry_run;
    let outcome = match crate::publish_core::publish_dir(
        &root,
        std::path::Path::new(path),
        branch.as_deref(),
        opts,
    ) {
        Ok(o) => o,
        Err(crate::publish_core::PublishError::TypeCheck(errs)) => {
            emit_publish_gate_failure("type-check", &errs, fmt);
            std::process::exit(2);
        }
        Err(crate::publish_core::PublishError::Examples(errs)) => {
            emit_publish_gate_failure("examples", &errs, fmt);
            std::process::exit(2);
        }
        Err(e) => return Err(e.into_anyhow()),
    };
    match outcome {
        crate::publish_core::Outcome::DryRun(d) => {
            let actions: Vec<serde_json::Value> = d
                .op_kinds
                .iter()
                .map(|k| serde_json::to_value(k).unwrap())
                .collect();
            acli::emit_dry_run(
                "publish",
                fmt,
                &format!("would apply {} op(s) to branch {}", d.op_kinds.len(), d.branch),
                actions,
            );
            Ok(())
        }
        crate::publish_core::Outcome::Published(p) => {
            let data = serde_json::json!({
                "ops": p.ops,
                "head_op": p.head_op,
                "signed_by": p.signed_by,
                // The recorded Intent's id, so a harness (lex-code) can hand
                // it to `lex recall` / `lex op replay`.
                "intent_id": p.intent_id,
                "files_manifest": p.files_manifest,
            });
            acli::emit_or_text("publish", data, fmt, || {});
            Ok(())
        }
    }
}

/// Render a rejected publish (`phase` = `type-check` | `examples`): the ACLI
/// envelope on stdout in JSON mode, one diagnostic JSON object per line on
/// stderr in text mode. The caller exits 2.
fn emit_publish_gate_failure(phase: &str, errs: &[lex_types::TypeError], fmt: &OutputFormat) {
    let arr: Vec<serde_json::Value> = errs
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    let data = serde_json::json!({ "phase": phase, "errors": arr });
    acli::emit_or_text("publish", data, fmt, || {
        for e in errs {
            if let Ok(j) = serde_json::to_string(e) {
                eprintln!("{j}");
            }
        }
    });
}

/// Record the caller's Intent (prompt / model / session / issue) for a
/// publish-shaped write — `lex publish` and `lex files commit` share this so
/// both attribute their ops the same way.
///
/// #970: unconditional. Intent used to be opt-in, and the result was that
/// the hosted corpus reached 136k ops with ZERO intents — the "why was this
/// changed" provenance that distinguishes lex-vcs from git had no data at
/// all on real history. A write without `--intent-prompt` now records an
/// explicitly *unattributed* intent instead of none: it does not invent a
/// prompt, but it does record the session (so one run's ops group
/// together), the producer, and a marker that no prompt was declared —
/// queryable, so "show me the ops nobody explained" becomes answerable.
pub(crate) fn record_intent(
    root: &std::path::Path,
    prompt: Option<String>,
    model: Option<String>,
    session: Option<String>,
    issue: Option<String>,
) -> Result<Option<lex_vcs::IntentId>> {
    let intent = build_intent(prompt, model, session, issue);
    lex_vcs::IntentLog::open(root)
        .with_context(|| "opening intent log")?
        .put(&intent)
        .with_context(|| "recording intent")?;
    Ok(Some(intent.intent_id.clone()))
}

/// The Intent a publish-shaped CLI write carries, built (not recorded) from its
/// `--intent-*` flags — see [`record_intent`] for the unattributed default.
/// Split out so `lex publish` can hand it to the shared publish core (#892 PR 1),
/// which records it at the same point in the pipeline as before.
pub(crate) fn build_intent(
    prompt: Option<String>,
    model: Option<String>,
    session: Option<String>,
    issue: Option<String>,
) -> lex_vcs::Intent {
    let prompt = prompt.unwrap_or_else(|| UNATTRIBUTED_PROMPT.to_string());
    // `split_model_ref(None)` already yields this toolchain's spelling for
    // "the CLI made this, no model declared" (`cli/unknown`), which is the
    // honest descriptor for an unattributed write too.
    let (provider, name) = split_model_ref(model.as_deref());
    let intent = lex_vcs::Intent::new(
        prompt,
        session.unwrap_or_else(default_intent_session),
        lex_vcs::ModelDescriptor { provider, name, version: None },
        None,
    );
    match issue {
        Some(id) => intent.with_issue(id),
        None => intent,
    }
}

/// `provider/name` → `(provider, name)`. A bare name is attributed to
/// provider `cli`; `None` → `("cli", "unknown")`. The model ref is
/// recorded for audit and feeds the content-addressed IntentId, so the
/// default must be stable, not empty.
/// The prompt recorded when a publish declares no `--intent-prompt` (#970).
///
/// Deliberately *not* a plausible-looking prompt: it must be impossible to
/// mistake a synthesized intent for one a caller actually supplied. It is a
/// fixed string so it is exactly matchable — `lex recall --predicate` can list
/// every unattributed op, which is what makes "who never explained their
/// changes" answerable instead of invisible.
pub(crate) const UNATTRIBUTED_PROMPT: &str = "(unattributed: published without --intent-prompt)";

/// The session id for a publish that gave no `--intent-session` (#970).
///
/// `LEX_INTENT_SESSION` lets a harness supply continuity across several `lex`
/// invocations that belong to one logical run. Without it, the id is
/// per-process: one invocation's ops group together, while separate runs stay
/// separate. A *constant* default would be worse than none — it would collapse
/// every publish ever made on the machine into a single bogus "session", making
/// `lex recall --session` useless precisely where it should help.
fn default_intent_session() -> String {
    if let Ok(s) = std::env::var("LEX_INTENT_SESSION") {
        if !s.trim().is_empty() {
            return s;
        }
    }
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("cli-{}-{started}", std::process::id())
}

fn split_model_ref(m: Option<&str>) -> (String, String) {
    match m {
        None => ("cli".to_string(), "unknown".to_string()),
        Some(s) => match s.split_once('/') {
            Some((p, n)) if !p.is_empty() && !n.is_empty() => (p.to_string(), n.to_string()),
            _ => ("cli".to_string(), s.to_string()),
        },
    }
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
/// description + signature + examples. `--include-draft` also ranks
/// functions that only have a Draft stage (every stage of a pulled
/// store, #969); without it, skipped drafts are counted and reported
/// as a hint rather than silently yielding 0 hits. Slice 1 ships only the
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
    const USAGE: &str = "usage: lex store search [--limit N] [--include-draft] \"<query>\"";
    let mut limit: usize = 10;
    let mut include_draft = false;
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
            "--include-draft" => include_draft = true,
            other if !other.starts_with("--") => {
                if query.is_some() {
                    bail!("{USAGE}");
                }
                query = Some(other.to_string());
            }
            other => bail!("unknown flag `{other}` for `lex store search`"),
        }
    }
    let query = query.ok_or_else(|| anyhow!("{USAGE}"))?;

    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = build_embedder(&root)?;
    // Where a function has several Draft versions, index the one the current
    // branch head names. Best-effort: a store with no head just falls back to
    // the newest Draft.
    let prefer = if include_draft {
        store
            .branch_head(&store.current_branch())
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let opts = lex_search::BuildOptions {
        include_draft,
        prefer,
    };
    let idx = lex_search::SearchIndex::build_with(&store, &*embedder, &opts)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let hits = idx
        .query(&*embedder, &query, limit)
        .map_err(|e| anyhow!("query embedding: {e}"))?;
    // #969: never let skipped Drafts read as "no matches". Lifecycle status is
    // not carried by `op pull`, so every stage of a pulled store is Draft and
    // an Active-only search sees none of it.
    let hint = (idx.drafts_skipped > 0).then(|| {
        format!(
            "{} draft function(s) not searched (only Active stages are ranked by default; \
             a pulled store is all Draft) -- re-run with --include-draft to include them",
            idx.drafts_skipped
        )
    });
    let mut v = serde_json::json!({
        "query": &query,
        "limit": limit,
        "include_draft": include_draft,
        "indexed": idx.stages.len(),
        "drafts_skipped": idx.drafts_skipped,
        "hits": serde_json::to_value(&hits)?,
    });
    if let Some(h) = &hint {
        v["hint"] = serde_json::Value::String(h.clone());
    }
    acli::emit_or_text("store-search", v.clone(), fmt, || {
        println!("{} hit(s) for `{}`", hits.len(), query);
        for h in &hits {
            let status = if h.status == lex_store::StageStatus::Active {
                String::new()
            } else {
                format!("  [{}]", format!("{:?}", h.status).to_lowercase())
            };
            println!(
                "  {:>6.3}  {}::{}  {}{}",
                h.score.fused, h.stage_id, h.name, h.signature, status,
            );
            if let Some(d) = &h.description {
                println!("          note: {d}");
            }
        }
        if let Some(h) = &hint {
            println!("hint: {h}");
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
