//! The shared publish core (#892 PR 1): everything `lex publish <path>` does
//! between "the flags are parsed" and "print the result", as a function that
//! neither prints nor exits.
//!
//! `cmd_publish` (`store.rs`) is now a thin wrapper — parse flags, build the
//! [`lex_vcs::Intent`], call [`publish_dir`], map [`PublishError`] to the same
//! stderr text and `exit(2)` as before. `lex op import-git` (later PRs) calls
//! the very same function with a deterministic Intent, so tree → ops and every
//! gate are one implementation and the two commands can never disagree.
//!
//! Pipeline (order is load-bearing — it is what `lex publish` always did):
//! load → canonicalize → dependency-resolved type-check → examples gate →
//! *open the store* → read the old head → `compute_diff_with_types` → (dry run
//! stops here) → record the Intent → `publish_program_with_intent` → files
//! step (`SetFiles`, last) → committed lock → examples-passed attestations.
//! The store is opened only after both gates pass, so a rejected publish never
//! creates one.

use anyhow::{anyhow, Context};
use lex_ast::{canonicalize_program, Stage};
use lex_store::{BlobId, DepResolver, PublishOp, Store, StoreError};
use lex_vcs::{ImportMap, Intent, Keypair};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Knobs for one publish. The [`Intent`] is supplied by the caller: the CLI
/// builds it from `--intent-*` flags exactly as before; an importer passes a
/// deterministic one (no `cli-<pid>-<epoch>` session), which is what makes two
/// publishes of the same tree produce identical op ids.
pub(crate) struct PublishOptions<'a> {
    /// Recorded in the intent log and stamped on every op this call emits.
    pub intent: Intent,
    /// Make the published head the branch's active one (`--activate`).
    pub activate: bool,
    /// Run the behavioural `examples {}` gate (and record its verdicts).
    pub examples: bool,
    /// Capture the working copy's non-op-log files into a `SetFiles` op
    /// (directory publishes only). `--no-files` clears it.
    pub files: bool,
    /// Sign the emitted ops.
    pub signer: Option<&'a Keypair>,
    /// Plan only: run the gates and the diff, record and write nothing.
    pub dry_run: bool,
    /// Accept a package with no `.lex` sources under `src/` as the *empty
    /// program* (so `diff_to_ops` emits removals for everything at the head).
    /// Off for `lex publish`, where an empty package is a mistake and stays
    /// an error; on for an importer replaying a commit that deletes the
    /// package.
    pub allow_empty: bool,
}

impl<'a> PublishOptions<'a> {
    /// The defaults `lex publish <dir>` has with no flags.
    pub(crate) fn new(intent: Intent) -> Self {
        PublishOptions {
            intent,
            activate: false,
            examples: true,
            files: true,
            signer: None,
            dry_run: false,
            allow_empty: false,
        }
    }
}

/// What a publish did.
pub(crate) enum Outcome {
    /// `dry_run`: the ops that *would* be applied; nothing was written.
    DryRun(DryRun),
    Published(Published),
}

pub(crate) struct DryRun {
    pub branch: String,
    pub op_kinds: Vec<lex_vcs::OperationKind>,
}

pub(crate) struct Published {
    /// Every op this call produced, semantic ops first, `SetFiles` last.
    pub ops: Vec<PublishOp>,
    /// The branch head after this call (the existing head if it was a no-op).
    pub head_op: Option<lex_vcs::OpId>,
    /// Public key (hex) of the signer, if any.
    pub signed_by: Option<String>,
    pub intent_id: lex_vcs::IntentId,
    /// The files manifest this call recorded, if it changed.
    pub files_manifest: Option<BlobId>,
}

/// Why a publish did not happen.
pub(crate) enum PublishError {
    /// The head does not type-check (with its dependencies resolved).
    TypeCheck(Vec<lex_types::TypeError>),
    /// A behavioural `examples {}` case failed.
    Examples(Vec<lex_types::TypeError>),
    /// The source could not be read/parsed/loaded.
    Load(anyhow::Error),
    /// The store refused (an unknown branch, a gate at write time, IO, ...).
    Store(StoreError),
    /// Anything else, with its context chain intact (opening the store, the
    /// intent log, the files step, ...).
    Other(anyhow::Error),
}

impl From<StoreError> for PublishError {
    fn from(e: StoreError) -> Self {
        PublishError::Store(e)
    }
}

impl PublishError {
    /// Collapse into the `anyhow` error the CLI's top-level handler prints.
    /// `Load`/`Other` hand back the original chain, `Store` converts exactly
    /// as a bare `?` on a `StoreError` did, so messages are unchanged.
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            PublishError::Load(e) | PublishError::Other(e) => e,
            PublishError::Store(e) => e.into(),
            PublishError::TypeCheck(errs) => {
                anyhow!("type-check failed with {} error(s)", errs.len())
            }
            PublishError::Examples(errs) => {
                anyhow!("examples failed with {} error(s)", errs.len())
            }
        }
    }
}

/// Publish `path` — a package directory (every `src/**/*.lex` as one
/// prefix-mangled program, plus the files step) or a single `.lex` file — to
/// the store at `store_root`.
///
/// `branch` defaults to the store's current branch. The store is opened only
/// after the type-check and examples gates pass.
pub(crate) fn publish_dir(
    store_root: &Path,
    path: &Path,
    branch: Option<&str>,
    opts: PublishOptions<'_>,
) -> Result<Outcome, PublishError> {
    let path_str = path
        .to_str()
        .ok_or_else(|| PublishError::Load(anyhow!("non-UTF-8 path {}", path.display())))?;

    // A directory argument publishes the whole package (mangled, one op
    // log, no sibling-name collisions); a file argument is a single
    // module. `pkg_imports` is `Some` only for a package.
    // #930: load without inlining registry/git deps, so the op-log keeps the
    // `import` edges; the resolver below supplies their signatures to the gate.
    let (prog, pkg_imports, module_prefixes) =
        crate::store::read_publish_source_opt(path_str, /*inline_packages=*/ false, opts.allow_empty)
            .map_err(PublishError::Load)?;
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
        path.to_path_buf(),
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
        return Err(PublishError::TypeCheck(errs));
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
    if opts.examples {
        let example_stages = if modules.is_empty() {
            stages.clone()
        } else {
            let (inlined_prog, _, _) = crate::store::read_publish_source_opt(
                path_str,
                /*inline_packages=*/ true,
                opts.allow_empty,
            )
            .map_err(PublishError::Load)?;
            let mut s = canonicalize_program(&inlined_prog);
            // Rewrite parse calls in the inlined view too (deps inlined → no
            // resolver needed); a type error here would have surfaced above.
            let _ = lex_types::check_and_rewrite_program(&mut s);
            s
        };
        let example_errors = lex_runtime::evaluate_examples(&example_stages);
        if !example_errors.is_empty() {
            return Err(PublishError::Examples(example_errors));
        }
    }

    let mut store = Store::open(store_root)
        .with_context(|| format!("opening store at {}", store_root.display()))
        .map_err(PublishError::Other)?;
    // #930 P2b-3: install the same client resolver on the store, so the
    // write-time gate resolves this head's external deps exactly as the
    // pre-check above did.
    store.set_dep_resolver(resolver);
    let branch = match branch {
        Some(b) => b.to_string(),
        None => store.current_branch(),
    };
    publish_stages(&store, path, &branch, &stages, pkg_imports, &module_prefixes, opts)
}

/// The second half of [`publish_dir`], for a caller that already holds a
/// configured store (dependency resolver installed) and a program it has
/// already loaded and type-checked — e.g. an importer that builds the empty
/// program for a commit deleting the package without touching the disk.
///
/// `stages` must be canonicalized and type-checked; the store's write-time
/// gate still runs, but this function does not repeat the pre-gates.
/// `pkg_imports` is the per-file import map of a package (`None` for a single
/// file: imports are derived from the `Import` stages). `path` is the source
/// path: when it is a directory the files step scans it and the committed lock
/// is read from beside its `lex.toml`.
pub(crate) fn publish_stages(
    store: &Store,
    path: &Path,
    branch: &str,
    stages: &[Stage],
    pkg_imports: Option<ImportMap>,
    module_prefixes: &BTreeMap<String, String>,
    opts: PublishOptions<'_>,
) -> Result<Outcome, PublishError> {
    let branch = branch.to_string();

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
            for s in stages {
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

    if opts.dry_run {
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
            new_stages: stages,
            new_imports: &new_imports,
            diff: &report,
            module_prefixes,
        })
        .map_err(|e| PublishError::Other(anyhow!("diff_to_ops: {e}")))?;
        return Ok(Outcome::DryRun(DryRun { branch, op_kinds }));
    }

    // #131 / #839 / #970: record the caller's Intent (prompt / model /
    // session) so every op this publish emits carries *why* it happened.
    // See `record_intent`'s doc comment for why this is unconditional.
    lex_vcs::IntentLog::open(store.root())
        .with_context(|| "opening intent log")
        .map_err(PublishError::Other)?
        .put(&opts.intent)
        .with_context(|| "recording intent")
        .map_err(PublishError::Other)?;
    let intent_id = opts.intent.intent_id.clone();

    let outcome = store.publish_program_with_intent(
        &branch,
        stages,
        &report,
        &new_imports,
        opts.activate,
        opts.signer,
        Some(intent_id.clone()),
        module_prefixes,
    )?;

    // #1007 PR 4: capture the working copy's non-op-log files into a
    // `SetFiles` op — last, under the same intent as the semantic ops above.
    // A directory publish only; a single-file publish is unaffected (there is
    // no package directory to scan). `apply_set_files` reads the branch's
    // CURRENT head at call time, so this correctly parents on whatever
    // `publish_program_with_intent` just produced (or the pre-existing head,
    // when nothing semantic changed).
    let mut ops_out = outcome.ops.clone();
    let mut files_op: Option<lex_vcs::OpId> = None;
    let mut files_manifest_id: Option<BlobId> = None;
    if opts.files && path.is_dir() {
        if let Some((op_id, manifest_id)) = crate::files::publish_files_if_changed(
            store,
            &branch,
            path,
            Some(intent_id.clone()),
        )
        .with_context(|| "capturing files manifest")
        .map_err(PublishError::Other)?
        {
            ops_out.push(PublishOp {
                op_id: op_id.clone(),
                kind: serde_json::to_value(&lex_vcs::OperationKind::SetFiles {
                    manifest: manifest_id.clone(),
                })
                .expect("SetFiles serializes"),
            });
            files_op = Some(op_id);
            files_manifest_id = Some(manifest_id);
        }
    }
    let final_head = files_op.clone().or_else(|| outcome.head_op.clone());
    // Did THIS call actually produce (or is it producing) `final_head`? Only
    // then may it rewrite that head's committed lock.
    let produced_new_head = !outcome.ops.is_empty() || files_op.is_some();

    // #1007 §0 / #930 P2b-1: capture the committed `lex.lock` at this head,
    // so a peer (the hub's write-time gate) can resolve this head's
    // dependencies against the exact pinned versions/heads it was built
    // with, rather than inlining them.
    //
    // THE FIX (#1007 §0): a no-op publish used to call `set_committed_lock`
    // on `outcome.head_op` regardless — and `publish_program_with_intent`
    // returns the *existing* head, unchanged, when it applies zero ops. So a
    // republish of an already-pushed, unchanged package silently rewrote
    // that head's lock (to whatever `lex.lock` happens to be on disk right
    // now, which may have drifted since the head was actually published) —
    // corrupting a record a peer may already be relying on. Gate the write
    // on `produced_new_head`: only a call that itself created `final_head`
    // (via a semantic op or the `SetFiles` above) may set its lock.
    //
    // Best-effort on the read (a dependency-free package has no lock to
    // commit); a store write error is real and propagates.
    if produced_new_head {
        if let Some(head) = final_head.as_deref() {
            if let Some((_toml, dir)) = lex_syntax::find_manifest(path) {
                if let Ok(lock_toml) = std::fs::read_to_string(dir.join("lex.lock")) {
                    store.set_committed_lock(head, &lock_toml)?;
                }
            }
        }
    }
    // #835 Tier 1: record the behavioral-examples verdict for each
    // published fn-stage that declares examples. Best-effort.
    if opts.examples {
        let op_for_stage: BTreeMap<String, String> = outcome
            .ops
            .iter()
            .filter_map(|op| {
                op.kind
                    .get("stage_id")
                    .and_then(|v| v.as_str())
                    .map(|sid| (sid.to_string(), op.op_id.clone()))
            })
            .collect();
        for stage in stages {
            let Stage::FnDecl(fd) = stage else { continue };
            if fd.examples.is_empty() {
                continue;
            }
            let Some(sid) = lex_ast::stage_id(stage) else { continue };
            if let Some(op_id) = op_for_stage
                .get(&sid)
                .cloned()
                .or_else(|| outcome.head_op.clone())
            {
                let _ = store.record_examples_passed(&sid, &op_id, fd.examples.len());
            }
        }
    }
    Ok(Outcome::Published(Published {
        ops: ops_out,
        head_op: final_head,
        signed_by: opts.signer.map(|kp| kp.public_hex()),
        intent_id,
        files_manifest: files_manifest_id,
    }))
}
