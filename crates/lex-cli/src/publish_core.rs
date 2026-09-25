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
#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    //! `publish_dir`-level properties the importer (#892 PR 4+) relies on. The
    //! byte-for-byte "nothing changed for `lex publish`" proof lives in
    //! `tests/publish_core_892.rs`; these pin the *new* capabilities.

    use super::*;
    use lex_store::files::{is_reserved_path, MODE_EXEC, MODE_FILE};
    const ERROR_LEX: &str = "type Err = { code :: Int, msg :: Str }\n\n\
        fn format(e :: Err) -> Str {\n  e.msg\n}\n";
    const LIB_LEX: &str = "import \"./error\" as e\n\n\
        fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n\n\
        fn double(n :: Int) -> Int {\n  n * 2\n}\n";

    fn write(dir: &Path, name: &str, contents: &[u8]) {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn package(dir: &Path) {
        write(dir, "lex.toml", b"[package]\nname = \"corepkg\"\nversion = \"0.1.0\"\n");
        write(dir, "src/error.lex", ERROR_LEX.as_bytes());
        write(dir, "src/lib.lex", LIB_LEX.as_bytes());
        write(dir, "README.md", b"# corepkg\n");
        write(dir, "bin/run.sh", b"#!/bin/sh\necho hi\n");
        write(dir, "logo.bin", &[0, 1, 2, 0xff, 0xfe]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dir.join("bin/run.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }

    /// A deterministic Intent of the kind the importer passes: fixed session,
    /// fixed model, no `cli-<pid>-<epoch>`.
    fn import_intent(created_at: u64) -> Intent {
        Intent::with_timestamp(
            "the commit message",
            "git-import:deadbeef",
            lex_vcs::ModelDescriptor {
                provider: "git".into(),
                name: "import".into(),
                version: Some("1".into()),
            },
            None,
            created_at,
        )
    }

    fn published(o: Outcome) -> Published {
        match o {
            Outcome::Published(p) => p,
            Outcome::DryRun(_) => panic!("expected a real publish"),
        }
    }

    fn publish(root: &Path, dir: &Path, opts: PublishOptions<'_>) -> Result<Published, PublishError> {
        publish_dir(root, dir, None, opts).map(published)
    }

    fn err_text(e: PublishError) -> String {
        format!("{:#}", e.into_anyhow())
    }

    fn op_ids(p: &Published) -> Vec<String> {
        p.ops.iter().map(|o| o.op_id.clone()).collect()
    }

    #[test]
    fn same_inputs_and_intent_give_identical_op_ids_across_stores_and_dirs() {
        let t = tempfile::tempdir().unwrap();
        let (dir_a, dir_b) = (t.path().join("a"), t.path().join("b-elsewhere"));
        package(&dir_a);
        package(&dir_b);
        let (store_a, store_b) = (t.path().join("store_a"), t.path().join("store_b"));

        // Different `created_at` (unhashed) and a different checkout path
        // and store path: none of it may reach an op id.
        let a = publish(&store_a, &dir_a, PublishOptions::new(import_intent(1))).unwrap();
        let b = publish(&store_b, &dir_b, PublishOptions::new(import_intent(999))).unwrap();

        assert!(!a.ops.is_empty());
        assert_eq!(op_ids(&a), op_ids(&b), "identical inputs must give identical ops");
        assert_eq!(a.head_op, b.head_op);
        assert_eq!(a.intent_id, b.intent_id);
        assert_eq!(a.files_manifest, b.files_manifest);
        assert!(a.files_manifest.is_some(), "a directory publish captures files");

        // The caller's intent is stamped on EVERY op — semantic ops and SetFiles alike.
        let log = lex_vcs::OpLog::open(&store_a).unwrap();
        let recs = log.walk_forward(a.head_op.as_ref().unwrap(), None).unwrap();
        assert_eq!(recs.len(), a.ops.len());
        assert!(
            recs.iter().all(|r| r.op.intent_id.as_ref() == Some(&a.intent_id)),
            "every op carries the caller-supplied intent"
        );

        // Republishing into the same store with the same intent is a no-op.
        let again = publish(&store_a, &dir_a, PublishOptions::new(import_intent(1))).unwrap();
        assert!(again.ops.is_empty(), "unchanged republish must emit zero ops");
        assert_eq!(again.head_op, a.head_op);
    }

    #[test]
    fn a_different_intent_changes_the_op_ids() {
        // Negative control for the test above: the intent IS hashed, so the
        // equality there is a real property and not an artefact of ignoring it.
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("pkg");
        package(&dir);
        let a = publish(&t.path().join("s1"), &dir, PublishOptions::new(import_intent(1))).unwrap();
        let other = Intent::with_timestamp(
            "another message",
            "git-import:deadbeef",
            lex_vcs::ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) },
            None,
            1,
        );
        let b = publish(&t.path().join("s2"), &dir, PublishOptions::new(other)).unwrap();
        assert_ne!(op_ids(&a), op_ids(&b));
    }

    /// The op kinds' `op` tags of `p.ops`.
    fn kinds(p: &Published) -> Vec<String> {
        p.ops
            .iter()
            .map(|o| o.kind["op"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn empty_program_on_a_non_empty_head_emits_removals_and_a_valid_empty_head() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = (t.path().join("pkg"), t.path().join("store"));
        package(&dir);
        let first = publish(&root, &dir, PublishOptions::new(import_intent(1))).unwrap();
        assert!(kinds(&first).contains(&"add_function".to_string()));
        let store = Store::open(&root).unwrap();
        assert_eq!(store.branch_head("main").unwrap().len(), 4, "double, render, format + the Err type");

        // The commit deletes every src/**/*.lex but keeps lex.toml and the rest.
        std::fs::remove_dir_all(dir.join("src")).unwrap();

        // `lex publish` semantics are untouched: an empty package is an error.
        let refused = publish(&root, &dir, PublishOptions::new(import_intent(2)));
        assert!(
            err_text(refused.err().expect("must be refused")).contains("has no src/ directory"),
            "without allow_empty the empty package is still the same error"
        );
        assert_eq!(store.branch_head("main").unwrap().len(), 4, "a refused publish writes nothing");

        let mut opts = PublishOptions::new(import_intent(2));
        opts.allow_empty = true;
        let del = publish(&root, &dir, opts).unwrap();
        let ks = kinds(&del);
        assert!(ks.iter().any(|k| k == "remove_function"), "removals emitted: {ks:?}");
        assert!(ks.iter().any(|k| k == "remove_type"), "the type is removed too: {ks:?}");
        // Only `src/**/*.lex` left: the files manifest is unchanged, so no SetFiles.
        assert!(!ks.iter().any(|k| k == "set_files"), "manifest untouched: {ks:?}");

        // A valid, empty head: no live declarations, no derived imports, and
        // a further identical publish is a true no-op.
        assert!(store.branch_head("main").unwrap().is_empty());
        assert!(ks.iter().any(|k| k == "remove_import"), "the file's import goes too: {ks:?}");
        assert!(
            store.derive_imports_from_oplog("main").unwrap().values().all(|s| s.is_empty()),
            "no import survives the deletion"
        );
        assert_eq!(store.get_branch("main").unwrap().unwrap().head_op, del.head_op);
        let mut opts = PublishOptions::new(import_intent(2));
        opts.allow_empty = true;
        let again = publish(&root, &dir, opts).unwrap();
        assert!(again.ops.is_empty(), "republishing the empty program is a no-op");

        // ...and the package can come back: the head is not wedged.
        package(&dir);
        let back = publish(&root, &dir, PublishOptions::new(import_intent(3))).unwrap();
        assert!(kinds(&back).contains(&"add_function".to_string()));
        assert_eq!(store.branch_head("main").unwrap().len(), 4);
    }

    #[test]
    fn empty_program_when_the_whole_package_is_gone() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = (t.path().join("pkg"), t.path().join("store"));
        package(&dir);
        publish(&root, &dir, PublishOptions::new(import_intent(1))).unwrap();

        // Only a README survives: no lex.toml, no src/.
        std::fs::remove_dir_all(dir.join("src")).unwrap();
        std::fs::remove_file(dir.join("lex.toml")).unwrap();

        let mut opts = PublishOptions::new(import_intent(2));
        opts.allow_empty = true;
        let del = publish(&root, &dir, opts).unwrap();
        let ks = kinds(&del);
        assert!(ks.iter().any(|k| k == "remove_function"), "{ks:?}");
        // lex.toml left the manifest too: one SetFiles, last, under the same intent.
        assert_eq!(ks.last().map(String::as_str), Some("set_files"), "{ks:?}");
        assert!(Store::open(&root).unwrap().branch_head("main").unwrap().is_empty());
    }

    #[test]
    fn allow_empty_never_papers_over_sources_that_exist() {
        // lex.toml missing but .lex sources present is still the manifest
        // error, not a silent "empty program" that would delete the package.
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = (t.path().join("pkg"), t.path().join("store"));
        package(&dir);
        std::fs::remove_file(dir.join("lex.toml")).unwrap();
        let mut opts = PublishOptions::new(import_intent(1));
        opts.allow_empty = true;
        let e = publish(&root, &dir, opts).err().expect("must be refused");
        assert!(err_text(e).contains("a package publish needs it"));
    }

    #[test]
    fn manifest_from_files_matches_build_manifest_on_the_working_copy() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = (t.path().join("pkg"), t.path().join("store"));
        package(&dir);
        let store = Store::open(&root).unwrap();

        // The bytes a git-object reader would hand over: every non-reserved
        // file, with its mode, in an order unrelated to the scan's.
        let mut entries: Vec<(String, &'static str, Vec<u8>)> = vec![
            ("logo.bin".into(), MODE_FILE, vec![0, 1, 2, 0xff, 0xfe]),
            ("bin/run.sh".into(), MODE_EXEC, b"#!/bin/sh\necho hi\n".to_vec()),
            ("README.md".into(), MODE_FILE, b"# corepkg\n".to_vec()),
            ("lex.toml".into(), MODE_FILE, b"[package]\nname = \"corepkg\"\nversion = \"0.1.0\"\n".to_vec()),
        ];
        entries.reverse();
        assert!(entries.iter().all(|(p, _, _)| !is_reserved_path(p)));

        let scanned = crate::files::build_manifest(&store, &dir).unwrap();
        let from_bytes = crate::files::manifest_from_files(&store, entries).unwrap();
        assert_eq!(from_bytes.id(), scanned.id(), "same bytes + modes => same manifest id");
        assert_eq!(from_bytes.entries.len(), 4);
        assert!(
            !scanned.entries.keys().any(|k| k.starts_with("src/")),
            "the scan excludes reserved src/**/*.lex"
        );

        // A different mode is a different manifest (negative control).
        let flipped = crate::files::manifest_from_files(
            &store,
            vec![("bin/run.sh".to_string(), MODE_FILE, b"#!/bin/sh\necho hi\n".to_vec())],
        )
        .unwrap();
        assert_ne!(flipped.id(), from_bytes.id());
        // Reserved paths are rejected loudly, not dropped.
        assert!(crate::files::manifest_from_files(
            &store,
            vec![("src/x.lex".to_string(), MODE_FILE, b"fn f() -> Int { 1 }".to_vec())],
        )
        .is_err());
    }

    #[test]
    fn publish_manifest_if_changed_records_once_then_is_a_no_op() {
        let t = tempfile::tempdir().unwrap();
        let (dir, root) = (t.path().join("pkg"), t.path().join("store"));
        package(&dir);
        let p = publish(&root, &dir, PublishOptions::new(import_intent(1))).unwrap();
        let store = Store::open(&root).unwrap();

        // The manifest the publish captured is already in force: no new op.
        let m = crate::files::build_manifest(&store, &dir).unwrap();
        assert!(crate::files::publish_manifest_if_changed(&store, "main", &m, None)
            .unwrap()
            .is_none());

        // A changed manifest lands as exactly one SetFiles op on the head.
        let m2 = crate::files::manifest_from_files(
            &store,
            vec![("NOTES.md".to_string(), MODE_FILE, b"hi\n".to_vec())],
        )
        .unwrap();
        let (op, manifest_id) =
            crate::files::publish_manifest_if_changed(&store, "main", &m2, None).unwrap().unwrap();
        assert_eq!(manifest_id, m2.id());
        assert_eq!(store.get_branch("main").unwrap().unwrap().head_op, Some(op));
        assert_ne!(p.head_op, store.get_branch("main").unwrap().unwrap().head_op);
    }
}
