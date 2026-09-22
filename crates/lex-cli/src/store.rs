//! `lex publish` and `lex store *`: publishing stages into the content store, store maintenance and search.

use super::*;
use lex_store::DepResolver; // #930: `resolve_modules` on the client resolver
use lex_syntax::{load_package, Manifest};

/// Read the source `lex publish` was given. A **directory** is a whole
/// package: every `src/**/*.lex` is loaded as one prefix-mangled program
/// (`<stem>_<hash>.<name>`), so two modules can each declare a `validate`
/// without colliding in the branch's name-keyed type-check scope
/// (#828/#894) — publishing a multi-module package module-by-module can't
/// (they collide). Returns the per-file import map for a package; a
/// single **file** is loaded as before and returns `None` (the caller
/// derives imports from the parsed `Import` stages).
fn read_publish_source(
    path: &str,
    inline_packages: bool,
) -> Result<(SynProgram, Option<lex_vcs::ImportMap>, BTreeMap<String, String>)> {
    let p = std::path::Path::new(path);
    if !p.is_dir() {
        return Ok((read_program(path)?, None, BTreeMap::new()));
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
    use lex_vcs::ImportMap;

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
         [--intent-issue ISSUE_ID] <file>\n\
         \n\
         Every publish records an Intent. Without --intent-prompt it is recorded \
         as explicitly unattributed (#970) — pass --intent-prompt to say why the \
         change was made, which is what makes `lex recall` and `lex op replay` useful.")
    })?;
    let signer = resolve_signing_key(signing_key_flag.as_deref())?;

    // A directory argument publishes the whole package (mangled, one op
    // log, no sibling-name collisions); a file argument is a single
    // module. `pkg_imports` is `Some` only for a package.
    // #930: load without inlining registry/git deps, so the op-log keeps the
    // `import` edges; the resolver below supplies their signatures to the gate.
    let (prog, pkg_imports, module_prefixes) = read_publish_source(path, /*inline_packages=*/ false)?;
    // #168: type-check *and* rewrite stdlib parse calls so a
    // typed `toml.parse[T]` validates required fields before
    // returning Ok. The mutation lands in the canonical AST so
    // every downstream consumer (bytecode compile, store
    // publish) sees the strict shape.
    let mut stages = canonicalize_program(&prog);
    // #930: resolve this head's external dependency signatures from the
    // working copy, so the non-inlined head type-checks here exactly as the
    // store gate will; the same resolver is installed on the store below.
    let resolver = std::sync::Arc::new(crate::dep_resolver::ClientDepResolver::new(
        std::path::PathBuf::from(path),
    ));
    let modules = resolver.resolve_modules(&stages, None);
    let module_types = resolver.resolve_module_types(&stages, None);
    let dep_prefixes = resolver.resolve_module_prefixes(&stages, None);
    if let Err(errs) = lex_types::check_and_rewrite_program_with_deps(
        &mut stages,
        &modules,
        &module_types,
        &dep_prefixes,
    ) {
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

    // #835 Tier 1: behavioral example gate — run `examples {}` and refuse
    // the publish on any mismatch, the same hard-error contract `lex check`
    // uses. Type-level example checks already ran above.
    //
    // #930: examples RUN the code, so with external dependencies present they
    // need those dependencies' *implementations* — the non-inlined `stages`
    // above only carry the edges. Re-load with inlining for the example run;
    // the op-log still gets the non-inlined `stages`. A dependency-free publish
    // (the common case) skips the extra load — `stages` already inline
    // everything local.
    let example_stages = if modules.is_empty() {
        stages.clone()
    } else {
        let (inlined_prog, _, _) = read_publish_source(path, /*inline_packages=*/ true)?;
        let mut s = canonicalize_program(&inlined_prog);
        // Rewrite parse calls in the inlined view too (deps inlined → no
        // resolver needed); a type error here would have surfaced above.
        let _ = lex_types::check_and_rewrite_program(&mut s);
        s
    };
    let example_errors = lex_runtime::evaluate_examples(&example_stages);
    if !example_errors.is_empty() {
        let arr: Vec<serde_json::Value> = example_errors
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        let data = serde_json::json!({ "phase": "examples", "errors": arr });
        acli::emit_or_text("publish", data, fmt, || {
            for e in &example_errors {
                if let Ok(j) = serde_json::to_string(e) {
                    eprintln!("{j}");
                }
            }
        });
        std::process::exit(2);
    }

    let mut store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    // #930 P2b-3: install the same client resolver on the store, so the
    // write-time gate resolves this head's external deps exactly as the
    // pre-check above did.
    store.set_dep_resolver(resolver);
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // Compute the diff. We need the old fns and new fns.
    let old_head = store.branch_head(&branch)?;
    // Read every live declaration through the `SigId` the head names it
    // by — NOT by `StageId`. A `StageId` is name-independent, so two
    // structurally identical helpers copy-pasted across a package's
    // modules (`list_contains_str` in both `constraints.lex` and
    // `migrate.lex`) share one `StageId`; reading the old side by
    // `StageId` (`get_ast`) returns a single name for the pair and drops
    // the other, so the diff re-adds it on every republish — unbounded op
    // growth (#818/#826/#894). `get_asts_for_sigs_bulk` reads each
    // `SigId`'s own stored AST, recovering the correct name for each. The
    // HTTP publish path already reads the old side this way.
    let head_pairs: Vec<(String, String)> = old_head
        .iter()
        .map(|(sig, stage)| (sig.clone(), stage.clone()))
        .collect();
    let mut old_fns: BTreeMap<String, lex_ast::FnDecl> = BTreeMap::new();
    let mut old_types: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    for ast in store
        .get_asts_for_sigs_bulk(&head_pairs)
        .into_iter()
        .filter_map(|r| r.ok())
    {
        match ast {
            Stage::FnDecl(fd) => {
                old_fns.insert(fd.name.clone(), fd);
            }
            // Types too, so the op log captures `type`s (#895).
            Stage::TypeDecl(td) => {
                old_types.insert(td.name.clone(), td);
            }
            _ => {}
        }
    }
    let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|s| match s {
            Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let new_types: BTreeMap<String, lex_ast::TypeDecl> = stages
        .iter()
        .filter_map(|s| match s {
            Stage::TypeDecl(td) => Some((td.name.clone(), td.clone())),
            _ => None,
        })
        .collect();
    let report = lex_vcs::compute_diff_with_types(
        &old_fns, &new_fns, &old_types, &new_types, /* body_patches: */ true,
    );

    // Build the new imports map. A package publish already attributed
    // imports per source file (`src/schema.lex` → its modules); a single
    // file groups all its imports under one stable, transport-independent
    // `<source>` key so a CLI vs HTTP publish of the same file produces
    // identical op_ids.
    let new_imports: ImportMap = match pkg_imports {
        Some(im) => im,
        None => {
            let mut new_imports = ImportMap::new();
            let entry = new_imports.entry("<source>".to_string()).or_default();
            for s in &stages {
                if let Stage::Import(im) = s {
                    entry.insert(lex_vcs::ImportRef {
                        reference: im.reference.clone(),
                        alias: im.alias.clone(),
                    });
                }
            }
            new_imports
        }
    };

    if dry_run {
        // Compute the op kinds for the dry-run preview using diff_to_ops
        // directly, without persisting anything. `report` already
        // carries each entry's own resolved `old_sig_id` (computed by
        // `compute_diff` directly from the old FnDecl), so there's no
        // separate name-keyed sig lookup to build here — see
        // `diff_report`'s doc comments for why that used to be a bug
        // (#818).
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
            old_effects: &old_effects,
            old_imports: &old_imports,
            new_stages: &stages,
            new_imports: &new_imports,
            diff: &report,
            module_prefixes: &module_prefixes,
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

    // #131 / #839: record the caller's Intent (prompt / model / session) so
    // every op this publish emits carries *why* it happened.
    //
    // #970: this is no longer optional. Intent was opt-in, and the result was
    // that the hosted corpus reached 136k ops with ZERO intents — the "why was
    // this changed" provenance that distinguishes lex-vcs from git had no data
    // at all on real history. Optional provenance reliably converges on no
    // provenance, so a publish without `--intent-prompt` now records an
    // explicitly *unattributed* intent instead of none.
    //
    // It does not invent a prompt. What it does record is worth having: the
    // session (so one run's ops group together), the producer, and a marker
    // that no prompt was declared — which is queryable, so "show me the ops
    // nobody explained" becomes answerable rather than indistinguishable from
    // the rest of history.
    let intent_id: Option<lex_vcs::IntentId> = {
        let prompt = intent_prompt
            .clone()
            .unwrap_or_else(|| UNATTRIBUTED_PROMPT.to_string());
        // `split_model_ref(None)` already yields this toolchain's spelling for
        // "the CLI made this, no model declared" (`cli/unknown`), which is the
        // honest descriptor for an unattributed publish too.
        let (provider, name) = split_model_ref(intent_model.as_deref());
        let intent = lex_vcs::Intent::new(
            prompt,
            intent_session.clone().unwrap_or_else(default_intent_session),
            lex_vcs::ModelDescriptor { provider, name, version: None },
            None,
        );
        let intent = match &intent_issue {
            Some(id) => intent.with_issue(id.clone()),
            None => intent,
        };
        lex_vcs::IntentLog::open(&root)
            .with_context(|| "opening intent log")?
            .put(&intent)
            .with_context(|| "recording intent")?;
        Some(intent.intent_id.clone())
    };

    let outcome = store.publish_program_with_intent(
        &branch,
        &stages,
        &report,
        &new_imports,
        activate,
        signer.as_ref(),
        intent_id.clone(),
        &module_prefixes,
    )?;
    // #930 P2b-1: capture the committed `lex.lock` at this head, so a peer
    // (the hub's write-time gate) can resolve this head's dependencies
    // against the exact pinned versions/heads it was built with, rather than
    // inlining them. Best-effort on the read (a dependency-free package has no
    // lock to commit); a store write error is real and propagates.
    if let Some(head) = outcome.head_op.as_deref() {
        if let Some((_toml, dir)) = lex_syntax::find_manifest(std::path::Path::new(path)) {
            if let Ok(lock_toml) = std::fs::read_to_string(dir.join("lex.lock")) {
                store.set_committed_lock(head, &lock_toml)?;
            }
        }
    }
    // #835 Tier 1: record the behavioral-examples verdict for each
    // published fn-stage that declares examples. Best-effort.
    {
        use std::collections::BTreeMap;
        let op_for_stage: BTreeMap<String, String> = outcome.ops.iter()
            .filter_map(|op| op.kind.get("stage_id").and_then(|v| v.as_str())
                .map(|sid| (sid.to_string(), op.op_id.clone())))
            .collect();
        for stage in &stages {
            let Stage::FnDecl(fd) = stage else { continue };
            if fd.examples.is_empty() { continue; }
            let Some(sid) = lex_ast::stage_id(stage) else { continue };
            if let Some(op_id) = op_for_stage.get(&sid).cloned().or_else(|| outcome.head_op.clone()) {
                let _ = store.record_examples_passed(&sid, &op_id, fd.examples.len());
            }
        }
    }
    let signed = signer.as_ref().map(|kp| kp.public_hex());
    let data = serde_json::json!({
        "ops": outcome.ops,
        "head_op": outcome.head_op,
        "signed_by": signed,
        // The recorded Intent's id when --intent-prompt was given, so a
        // harness (lex-code) can hand it to `lex recall` / `lex op replay`.
        "intent_id": intent_id,
    });
    acli::emit_or_text("publish", data, fmt, || {});
    Ok(())
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
