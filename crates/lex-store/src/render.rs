//! Rendering a package's op-log head back to **source** — single `src.lex`
//! for a single-module package, or the de-flattened `src/*.lex` tree for a
//! multi-module one (#894).
//!
//! Declarations published through the package loader carry a per-file
//! mangling prefix (`schema_a1b2.validate`); each `AddFunction`/`AddType` op
//! records the source file it came from (`in_file`, #903). To render source
//! we group the head's stages by file, strip each file's own prefix, and
//! rewrite a reference to *another* file's prefix into `alias.name` plus a
//! local `import`.
//!
//! This lives in `lex-store` (not the CLI) so both `lex export-git` and the
//! hosted registry's archive endpoint render identically — the same source a
//! human reads in the git mirror is the source a consumer installs.

use std::collections::{BTreeMap, BTreeSet};

use lex_vcs::{default_import_alias, OpLog, OperationKind};

use crate::store::{SkippedStage, Store, StoreError};

/// A package head decomposed into what the renderer needs: the SigId→StageId
/// head map, each SigId's source file, and the imports (flat, and per-file).
#[derive(Debug, Default, Clone)]
pub struct PackageHead {
    /// SigId → StageId at the head.
    pub map: BTreeMap<String, String>,
    /// SigId → the source file its declaration came from (`in_file`).
    pub sig_files: BTreeMap<String, String>,
    /// module → alias, flattened across files (single-module render).
    pub flat_imports: BTreeMap<String, String>,
    /// file → (module → alias) (multi-module render).
    pub file_imports: BTreeMap<String, BTreeMap<String, String>>,
}

/// Rendered package source: one module, or a `relpath → source` tree.
///
/// The single-module arm carries the module's `path` when the head knows it,
/// for the same reason the multi arm does — so no caller has to invent one
/// (#988). Both callers used to hard-code a name here, and *different* ones
/// (`src/lib.lex` for the registry archive, `src.lex` for the git mirror),
/// which silently **renamed** any package whose module was not called `lib`:
/// `lex-jobs` ships `src/jobs.lex`, so dependents writing
/// `import "lex-jobs/src/jobs"` could not resolve a module that was present
/// under another name.
///
/// `path: None` means the head genuinely records no source path — every op
/// predates `in_file` — so there is nothing to recover and any name would be
/// a guess. Each caller then applies its *own* convention, which is why this
/// is an `Option` rather than a default filled in here: the two conventions
/// differ, and collapsing them would change the git mirror's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedSource {
    Single { path: Option<String>, src: String },
    Multi(BTreeMap<String, String>),
}

/// The file a single-module head's declarations were declared in, or `None`
/// when the head cannot say.
///
/// Note this reads the *partial* map deliberately. A head that mixes
/// pre-`in_file` ops with newer ones renders as `Single` (the multi-module
/// test demands `in_file` on **every** sig) while still knowing perfectly well
/// which file it came from — which is exactly the shape a legacy package takes
/// when it is re-pushed onto its existing op-log.
fn single_module_path(head: &PackageHead) -> Option<String> {
    let distinct: std::collections::BTreeSet<&String> =
        head.sig_files.values().filter(|f| !f.is_empty()).collect();
    match distinct.len() {
        1 => Some(distinct.into_iter().next().expect("length checked").clone()),
        // Nothing recorded, or — defensively — several despite a single-module
        // render. Neither can name a file honestly.
        _ => None,
    }
}

/// Walk the op-log from `head_op` and assemble the [`PackageHead`] — the same
/// bookkeeping `lex export-git` does incrementally, done once for a single
/// head (used by the registry archive endpoint).
pub fn package_head_at_op(store: &Store, head_op: &str) -> Result<PackageHead, StoreError> {
    let log = OpLog::open(store.root())?;
    let mut head = PackageHead::default();
    for rec in log.walk_forward(&head_op.to_string(), None)? {
        crate::branches::apply_transition(&mut head.map, &rec.produces);
        match &rec.op.kind {
            OperationKind::AddFunction { sig_id, in_file: Some(f), .. }
            | OperationKind::AddType { sig_id, in_file: Some(f), .. } => {
                head.sig_files.insert(sig_id.clone(), f.clone());
            }
            OperationKind::AddImport { in_file, module, alias } => {
                let alias = alias.clone().unwrap_or_else(|| default_import_alias(module));
                head.flat_imports.insert(module.clone(), alias.clone());
                head.file_imports.entry(in_file.clone()).or_default().insert(module.clone(), alias);
            }
            OperationKind::RemoveImport { in_file, module } => {
                head.flat_imports.remove(module);
                if let Some(m) = head.file_imports.get_mut(in_file) {
                    m.remove(module);
                }
            }
            // #992: a sig-moving modification carries its file across exactly
            // as a rename does — otherwise the moved declaration loses its
            // `in_file`, and a multi-module package silently renders as one.
            OperationKind::RenameSymbol { from, to, .. }
            | OperationKind::ChangeEffectSig { sig_id: from, to_sig_id: Some(to), .. }
            | OperationKind::ModifyBody { sig_id: from, to_sig_id: Some(to), .. }
            | OperationKind::ModifyType { sig_id: from, to_sig_id: Some(to), .. } => {
                if let Some(f) = head.sig_files.remove(from) {
                    head.sig_files.insert(to.clone(), f);
                }
            }
            _ => {}
        }
    }
    Ok(head)
}

/// Render a package head to source. Multi-module iff every head fn/type stage
/// records its source file; otherwise a single module.
pub fn render_source(store: &Store, head: &PackageHead) -> Result<RenderedSource, StoreError> {
    render_source_inner(store, head).map_err(|e| match e {
        // #992: a head can already carry an entry no store can hold (written
        // before the write-time gate existed; releases are immutable). The
        // per-stage read reports that as a bare `unknown stage_id` — misleading,
        // since the stage *is* in the store, just under the sig its AST hashes
        // to. Diagnose only on failure, so a healthy render pays nothing.
        StoreError::UnknownStage(_) => store.check_pairs_satisfiable(&head.map).err().unwrap_or(e),
        other => other,
    })
}

fn render_source_inner(store: &Store, head: &PackageHead) -> Result<RenderedSource, StoreError> {
    let multi = !head.map.is_empty() && head.map.keys().all(|s| head.sig_files.contains_key(s));
    if multi {
        Ok(RenderedSource::Multi(render_multifile(store, head)?))
    } else {
        Ok(RenderedSource::Single {
            path: single_module_path(head),
            src: render_singlefile(store, head)?,
        })
    }
}

/// Extract a single-module package head's public function signatures as a
/// module record type — what the write-time gate hands to
/// [`lex_types::check_program_with_modules`] when it resolves a dependency
/// (#930 phase 2b). The dependency's op-log head is reconstructed,
/// de-mangled to bare names (reusing the same [`FileRewrite`] the single-file
/// renderer applies), and type-checked; each top-level function signature
/// becomes a field of the returned [`lex_types::Ty::Record`].
///
/// Only single-module dependencies are supported for now — a multi-module
/// head (every stage carries an `in_file`) returns
/// [`StoreError::UnsupportedMultiModuleDependency`] rather than silently
/// resolving the wrong surface; picking the imported module's file out of the
/// de-flattened tree is a later extension. The dependency must also
/// type-check on its own (a *leaf* — no unresolved dependencies of its own);
/// resolving a dependency that itself has registry/git dependencies is the
/// recursive extension that follows.
pub fn module_record_at_op(store: &Store, head_op: &str) -> Result<lex_types::Ty, StoreError> {
    module_record_at_op_for(store, head_op, None)
}

/// [`module_record_at_op`] scoped to **one module** of a (possibly multi-file)
/// package head (#942).
///
/// `module` is the `<module>` half of `import "<pkg>/<module>" as x`. With
/// `None` the head must be a single file — the pre-#942 behaviour, kept so
/// existing callers are unaffected.
///
/// A multi-file head can't simply be de-mangled wholesale: two files may each
/// define `validate` (#818), so flattening every prefix to a bare name would
/// collapse them onto one. Instead only the *requested* module is de-mangled;
/// its siblings stay under their own prefixes, which keeps the program
/// type-checkable (references from the requested module still resolve) and
/// makes that module's surface exactly the set of names with no prefix left.
pub fn module_record_at_op_for(
    store: &Store,
    head_op: &str,
    module: Option<&str>,
) -> Result<lex_types::Ty, StoreError> {
    let stages = match module {
        Some(m) => demangled_module_stages(store, head_op, m)?,
        None => demangled_head_stages(store, head_op)?,
    };
    let types = lex_types::check_program(&stages).map_err(StoreError::TypeError)?;
    // Every top-level function is part of the module's callable surface — and
    // after the scoped rewrite, "belongs to this module" is exactly "has no
    // mangle prefix left".
    let fields = types
        .fn_signatures
        .iter()
        .filter(|(name, _)| !name.contains('.'))
        .map(|(name, scheme)| (name.clone(), scheme.ty.clone()));
    Ok(lex_types::module_record_from_fields(fields))
}

/// The file in `files` that `module` names — `model` → `src/model.lex`, else
/// `model.lex`, else any file whose stem matches. Mirrors the loader's own
/// resolution order (`lex_syntax::workspace::find_module_file`) so a dependent
/// resolves the same file the compiler would.
fn module_file<'a>(files: impl Iterator<Item = &'a String>, module: &str) -> Option<String> {
    let want_src = format!("src/{module}.lex");
    let want_flat = format!("{module}.lex");
    let mut stem_match: Option<String> = None;
    for f in files {
        if *f == want_src || *f == want_flat {
            return Some(f.clone());
        }
        let stem = f.rsplit('/').next().unwrap_or(f).trim_end_matches(".lex");
        if stem == module && stem_match.is_none() {
            stem_match = Some(f.clone());
        }
    }
    stem_match
}

/// The head's stages with **one module** de-mangled to bare names and every
/// other file left under its own prefix (#942). See [`module_record_at_op_for`]
/// for why the siblings are deliberately not flattened.
pub(crate) fn demangled_module_stages(
    store: &Store,
    head_op: &str,
    module: &str,
) -> Result<Vec<lex_ast::Stage>, StoreError> {
    let head = package_head_at_op(store, head_op)?;
    let pairs: Vec<(String, String)> =
        head.map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let asts = store.get_asts_for_sigs_bulk(&pairs);

    let mut by_file: BTreeMap<String, Vec<lex_ast::Stage>> = BTreeMap::new();
    for ((sig, _), ast) in pairs.iter().zip(asts) {
        let stage = ast?;
        let file = head.sig_files.get(sig).cloned().unwrap_or_default();
        by_file.entry(file).or_default().push(stage);
    }

    let target = module_file(by_file.keys(), module)
        .ok_or(StoreError::UnsupportedMultiModuleDependency)?;

    let own_stages = by_file.get(&target).cloned().unwrap_or_default();
    let own_prefix = own_stages.iter().find_map(stage_prefix).unwrap_or_default();
    let mut bound_locals = BTreeSet::new();
    for s in &own_stages {
        collect_bound_locals(s, &mut bound_locals);
    }
    let mut rw = FileRewrite {
        own_prefix: &own_prefix,
        own_file: &target,
        prefix_to_file: &BTreeMap::new(),
        bound_locals: &bound_locals,
        local_imports: BTreeMap::new(),
        flatten_unknown_prefixes: false,
    };

    // The flattened head's imports are a single global alias map — the exact
    // namespace these stages were type-checked in when published.
    let mut stages: Vec<lex_ast::Stage> = head
        .flat_imports
        .iter()
        .map(|(reference, alias)| {
            lex_ast::Stage::Import(lex_ast::Import {
                reference: reference.clone(),
                alias: alias.clone(),
            })
        })
        .collect();
    // Rewrite EVERY stage, not just the target module's. The rewriter strips
    // only `own_prefix` (the target's) and — with no `prefix_to_file` and
    // flattening off — leaves every other prefix untouched. Applying it
    // wholesale therefore also fixes the siblings' *inbound* references: a
    // sibling calling `util_<hash>.helper` follows the declaration down to
    // bare `helper`. Rewriting only the target file left those references
    // dangling at a name that no longer existed.
    for file_stages in by_file.values() {
        for s in file_stages {
            let mut s = s.clone();
            rw.rewrite_stage(&mut s);
            stages.push(s);
        }
    }
    Ok(stages)
}

/// A single-file package head as a *de-mangled* canonical program: the
/// head's (stdlib) imports followed by its declarations under bare names —
/// `gcd`, not `lib_<hash>.gcd`. This is the program a dependent's author
/// sees, so it's the common substrate for everything that reasons about a
/// head at the source level: extracting a dependency's public surface
/// ([`module_record_at_op`]) and evaluating a typed issue's acceptance
/// against it (`crate::issues`, #949).
///
/// "Multi-module" means the head spans MORE THAN ONE source file — then
/// per-module de-mangling at the stage level isn't wired up yet (#942) and
/// this returns [`StoreError::UnsupportedMultiModuleDependency`]. A
/// single-file package still records an `in_file` for every stage when
/// published via `lex publish <dir>` (so it renders to `src/<file>.lex`), but
/// its whole surface is that one module; count distinct files rather than
/// "every stage has a file", which misclassified that case.
pub(crate) fn demangled_head_stages(
    store: &Store,
    head_op: &str,
) -> Result<Vec<lex_ast::Stage>, StoreError> {
    Ok(demangled_head_stages_impl(store, head_op, false)?.0)
}

/// [`demangled_head_stages`] with a choice of what to do about a head stage
/// the store can't load: fail (`skip_unloadable = false`), or drop it and
/// report it as a [`SkippedStage`] (#868 — replay over a long-lived history).
pub(crate) fn demangled_head_stages_impl(
    store: &Store,
    head_op: &str,
    skip_unloadable: bool,
) -> Result<(Vec<lex_ast::Stage>, Vec<SkippedStage>), StoreError> {
    let head = package_head_at_op(store, head_op)?;
    let distinct_files: BTreeSet<&String> = head.sig_files.values().collect();
    if distinct_files.len() > 1 {
        return Err(StoreError::UnsupportedMultiModuleDependency);
    }
    let pairs: Vec<(String, String)> =
        head.map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let (mut decls, skipped) = store.load_head_decls(&pairs, skip_unloadable)?;
    // De-mangle exactly as `render_singlefile` does.
    let own_prefix = decls.iter().find_map(stage_prefix).unwrap_or_default();
    let mut bound_locals = BTreeSet::new();
    for s in &decls {
        collect_bound_locals(s, &mut bound_locals);
    }
    let mut rw = FileRewrite {
        own_prefix: &own_prefix,
        own_file: "",
        prefix_to_file: &BTreeMap::new(),
        bound_locals: &bound_locals,
        local_imports: BTreeMap::new(),
        flatten_unknown_prefixes: true,
    };
    for s in &mut decls {
        rw.rewrite_stage(s);
    }
    let mut stages: Vec<lex_ast::Stage> = Vec::new();
    for (reference, alias) in &head.flat_imports {
        stages.push(lex_ast::Stage::Import(lex_ast::Import {
            reference: reference.clone(),
            alias: alias.clone(),
        }));
    }
    stages.extend(decls);
    Ok((stages, skipped))
}

/// The whole head as one source string (single module / #895 path). Imports
/// first, then the head stages read per-SigId (so structurally identical
/// stages that share a StageId keep their distinct names).
/// Restore each declaration's `#` comments from its stage metadata.
///
/// The stored AST carries none: `doc` is `serde(skip)` precisely so comments
/// cannot touch a SigId or StageId. They live in the stage's `Metadata`
/// instead — outside the hash, like `name` — and must be put back here, or
/// everything rendered from an op-log (registry archives, `lex export-git`)
/// arrives with all documentation stripped.
///
/// Read by `(sig, stage)`, never by stage id alone: two sigs can share a
/// StageId (#826), and an id-only lookup handed both declarations the same
/// metadata — which duplicated one module header across two declarations.
///
/// Best-effort per stage: a missing or unreadable record just means no
/// comments, never a failed render.
fn restore_doc(store: &Store, pairs: &[(String, String)], decls: &mut [lex_ast::Stage]) {
    for ((sig_id, stage_id), decl) in pairs.iter().zip(decls.iter_mut()) {
        let Ok(meta) = store.get_metadata_for_sig(sig_id, stage_id) else { continue };
        if meta.doc.is_empty() {
            continue;
        }
        match decl {
            lex_ast::Stage::FnDecl(fd) => fd.doc = meta.doc,
            lex_ast::Stage::TypeDecl(td) => td.doc = meta.doc,
            lex_ast::Stage::Import(_) => {}
        }
    }
}

fn render_singlefile(store: &Store, head: &PackageHead) -> Result<String, StoreError> {
    let pairs: Vec<(String, String)> = head.map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let mut decls: Vec<lex_ast::Stage> = Vec::new();
    for ast in store.get_asts_for_sigs_bulk(&pairs) {
        decls.push(ast?);
    }
    restore_doc(store, &pairs, &mut decls);
    // De-mangle: a single-module head still carries mangle prefixes (the
    // package loader mangles every declaration, and an inlined dependency
    // adds its own). With one module, everything belongs to this package's
    // namespace, so strip every mangle prefix to a bare name — otherwise the
    // rendered source has invalid dotted declarations (#930). FileRewrite with
    // an empty `prefix_to_file` and the shared own-prefix does exactly this
    // (own prefix stripped directly; any other mangle prefix via the inlined
    // fallback in `rename`).
    let own_prefix = decls.iter().find_map(stage_prefix).unwrap_or_default();
    let mut bound_locals = BTreeSet::new();
    for s in &decls {
        collect_bound_locals(s, &mut bound_locals);
    }
    let mut rw = FileRewrite {
        own_prefix: &own_prefix,
        own_file: "",
        prefix_to_file: &BTreeMap::new(),
        bound_locals: &bound_locals,
        local_imports: BTreeMap::new(),
        flatten_unknown_prefixes: true,
    };
    for s in &mut decls {
        rw.rewrite_stage(s);
    }

    let mut stages: Vec<lex_ast::Stage> = Vec::new();
    for (reference, alias) in &head.flat_imports {
        stages.push(lex_ast::Stage::Import(lex_ast::Import {
            reference: reference.clone(),
            alias: alias.clone(),
        }));
    }
    stages.extend(decls);
    Ok(lex_ast::print_stages(&stages))
}

/// De-flatten a mangled multi-module head into a `relpath → source` tree.
fn render_multifile(store: &Store, head: &PackageHead) -> Result<BTreeMap<String, String>, StoreError> {
    let mut prefix_to_file: BTreeMap<String, String> = BTreeMap::new();
    let mut by_file: BTreeMap<String, Vec<lex_ast::Stage>> = BTreeMap::new();
    // Read each stage through the SigId the head names it by (not by StageId,
    // which is name-independent) so cross-module structural twins keep their
    // own names and file (#818/#894).
    let pairs: Vec<(String, String)> = head.map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let asts = store.get_asts_for_sigs_bulk(&pairs);
    for ((sig, stage_id), ast) in pairs.iter().zip(asts) {
        let mut stage = ast?;
        // Put the declaration's comments back (see `restore_doc`): the stored
        // AST omits them so they cannot touch a hash.
        if let Ok(meta) = store.get_metadata_for_sig(sig, stage_id) {
            if !meta.doc.is_empty() {
                match &mut stage {
                    lex_ast::Stage::FnDecl(fd) => fd.doc = meta.doc,
                    lex_ast::Stage::TypeDecl(td) => td.doc = meta.doc,
                    lex_ast::Stage::Import(_) => {}
                }
            }
        }
        let file = head.sig_files.get(sig).cloned().unwrap_or_default();
        if let Some(prefix) = stage_prefix(&stage) {
            prefix_to_file.insert(prefix, file.clone());
        }
        by_file.entry(file).or_default().push(stage);
    }

    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (file, stages) in &by_file {
        let own_prefix = stages.iter().find_map(stage_prefix).unwrap_or_default();
        let mut bound_locals = BTreeSet::new();
        for s in stages {
            collect_bound_locals(s, &mut bound_locals);
        }
        let mut rw = FileRewrite {
            own_prefix: &own_prefix,
            own_file: file,
            prefix_to_file: &prefix_to_file,
            bound_locals: &bound_locals,
            local_imports: BTreeMap::new(),
            flatten_unknown_prefixes: true,
        };
        let rewritten: Vec<lex_ast::Stage> = stages
            .iter()
            .cloned()
            .map(|mut s| {
                rw.rewrite_stage(&mut s);
                s
            })
            .collect();

        let mut imports: BTreeMap<String, String> = head.file_imports.get(file).cloned().unwrap_or_default();
        imports.extend(rw.local_imports);

        let mut out_stages: Vec<lex_ast::Stage> = Vec::new();
        for (reference, alias) in &imports {
            out_stages.push(lex_ast::Stage::Import(lex_ast::Import {
                reference: reference.clone(),
                alias: alias.clone(),
            }));
        }
        out_stages.extend(rewritten);
        out.insert(file.clone(), lex_ast::print_stages(&out_stages));
    }
    Ok(out)
}

/// Whether `q` looks like a per-file mangle prefix (`<stem>_<hex6+>`), as
/// opposed to a stdlib import alias (`int`, `str`, `map`). Used to detect an
/// inlined dependency's prefix so it can be flattened to a bare name.
fn is_mangle_prefix(q: &str) -> bool {
    match q.rsplit_once('_') {
        Some((stem, hex)) => {
            !stem.is_empty()
                && hex.len() >= 6
                && hex.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// The mangling prefix of a declaration (`schema_a1b2.validate` →
/// `schema_a1b2`), or `None` for an import or an unmangled name.
fn stage_prefix(s: &lex_ast::Stage) -> Option<String> {
    let name = match s {
        lex_ast::Stage::FnDecl(fd) => &fd.name,
        lex_ast::Stage::TypeDecl(td) => &td.name,
        lex_ast::Stage::Import(_) => return None,
    };
    name.split_once('.').map(|(p, _)| p.to_string())
}

/// `("src/schema.lex", "src/error.lex")` → `("./error", "error")`.
fn relative_import(from: &str, to: &str) -> (String, String) {
    let from_dir: Vec<&str> = from
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let to_noext = to.strip_suffix(".lex").unwrap_or(to);
    let to_parts: Vec<&str> = to_noext.split('/').filter(|s| !s.is_empty()).collect();
    let alias = to_parts.last().copied().unwrap_or("mod").to_string();
    let mut i = 0;
    while i < from_dir.len() && i + 1 < to_parts.len() && from_dir[i] == to_parts[i] {
        i += 1;
    }
    let ups = from_dir.len() - i;
    let mut rel = String::new();
    if ups == 0 {
        rel.push_str("./");
    } else {
        for _ in 0..ups {
            rel.push_str("../");
        }
    }
    rel.push_str(&to_parts[i..].join("/"));
    (rel, alias)
}

fn collect_bound_locals(s: &lex_ast::Stage, out: &mut BTreeSet<String>) {
    if let lex_ast::Stage::FnDecl(fd) = s {
        for p in &fd.params {
            out.insert(p.name.clone());
        }
        collect_expr_locals(&fd.body, out);
        for ex in &fd.examples {
            for a in &ex.args {
                collect_expr_locals(a, out);
            }
            collect_expr_locals(&ex.expected, out);
        }
    }
}

fn collect_expr_locals(e: &lex_ast::CExpr, out: &mut BTreeSet<String>) {
    use lex_ast::CExpr::*;
    match e {
        Let { name, value, body, .. } => {
            out.insert(name.clone());
            collect_expr_locals(value, out);
            collect_expr_locals(body, out);
        }
        Lambda { params, body, .. } => {
            for p in params {
                out.insert(p.name.clone());
            }
            collect_expr_locals(body, out);
        }
        Match { scrutinee, arms } => {
            collect_expr_locals(scrutinee, out);
            for arm in arms {
                collect_pattern_locals(&arm.pattern, out);
                collect_expr_locals(&arm.body, out);
            }
        }
        Call { callee, args } => {
            collect_expr_locals(callee, out);
            for a in args {
                collect_expr_locals(a, out);
            }
        }
        Block { statements, result } => {
            for s in statements {
                collect_expr_locals(s, out);
            }
            collect_expr_locals(result, out);
        }
        Constructor { args, .. } => {
            for a in args {
                collect_expr_locals(a, out);
            }
        }
        RecordLit { fields } => {
            for f in fields {
                collect_expr_locals(&f.value, out);
            }
        }
        TupleLit { items } | ListLit { items } => {
            for i in items {
                collect_expr_locals(i, out);
            }
        }
        FieldAccess { value, .. } => collect_expr_locals(value, out),
        BinOp { lhs, rhs, .. } => {
            collect_expr_locals(lhs, out);
            collect_expr_locals(rhs, out);
        }
        UnaryOp { expr, .. } => collect_expr_locals(expr, out),
        Return { value } => collect_expr_locals(value, out),
        Var { .. } | Literal { .. } => {}
    }
}

fn collect_pattern_locals(p: &lex_ast::Pattern, out: &mut BTreeSet<String>) {
    use lex_ast::Pattern::*;
    match p {
        PVar { name } => {
            out.insert(name.clone());
        }
        PConstructor { args, .. } => {
            for a in args {
                collect_pattern_locals(a, out);
            }
        }
        PRecord { fields } => {
            for f in fields {
                collect_pattern_locals(&f.pattern, out);
            }
        }
        PTuple { items } => {
            for i in items {
                collect_pattern_locals(i, out);
            }
        }
        PLiteral { .. } | PWild => {}
    }
}

struct FileRewrite<'a> {
    own_prefix: &'a str,
    own_file: &'a str,
    prefix_to_file: &'a BTreeMap<String, String>,
    bound_locals: &'a BTreeSet<String>,
    local_imports: BTreeMap<String, String>,
    /// Whether a mangle prefix that maps to no file should be flattened to a
    /// bare name. True for every *source-rendering* path, where such a prefix
    /// is an inlined dependency the loader folded into this package's
    /// namespace and a dotted declaration would be invalid source.
    ///
    /// False when rewriting a single module of a multi-file head for its
    /// *type surface* (#942): there the sibling modules' declarations are
    /// still present under their own prefixes, so flattening them would
    /// collapse distinct functions onto one bare name (two files may each
    /// define `validate`, #818) and break the references that point at them.
    flatten_unknown_prefixes: bool,
}

impl FileRewrite<'_> {
    /// Un-mangle a dotted name for THIS file: own prefix → bare; another
    /// package file's prefix → `alias.rest` (recording the import); anything
    /// else (a stdlib alias like `int.to_str`, or a bare name) untouched.
    fn rename(&mut self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix(&format!("{}.", self.own_prefix)) {
            return rest.to_string();
        }
        if let Some((q, rest)) = name.split_once('.') {
            if q != self.own_prefix {
                if let Some(other_file) = self.prefix_to_file.get(q) {
                    let (import_ref, stem) = relative_import(self.own_file, other_file);
                    let alias = if self.bound_locals.contains(&stem) {
                        q.to_string()
                    } else {
                        stem
                    };
                    self.local_imports.insert(import_ref, alias.clone());
                    return format!("{alias}.{rest}");
                }
                // A mangle prefix (`<stem>_<hex>`) that maps to no file is an
                // *inlined dependency* — the loader flattened a registry dep
                // into this program (lex-lang#930). It has no file of its own,
                // so render it as a bare top-level name (inlining folds it into
                // this package's namespace); leaving `prefix.name` would emit
                // an invalid dotted declaration/reference. Stdlib aliases
                // (`int.to_str`) don't match the mangle pattern and pass through.
                if self.flatten_unknown_prefixes && is_mangle_prefix(q) {
                    return rest.to_string();
                }
            }
        }
        name.to_string()
    }

    fn rewrite_stage(&mut self, s: &mut lex_ast::Stage) {
        match s {
            lex_ast::Stage::FnDecl(fd) => {
                fd.name = self.rename(&fd.name);
                for p in &mut fd.params {
                    self.rewrite_type(&mut p.ty);
                }
                self.rewrite_type(&mut fd.return_type);
                self.rewrite_expr(&mut fd.body);
                for ex in &mut fd.examples {
                    for a in &mut ex.args {
                        self.rewrite_expr(a);
                    }
                    self.rewrite_expr(&mut ex.expected);
                }
            }
            lex_ast::Stage::TypeDecl(td) => {
                td.name = self.rename(&td.name);
                self.rewrite_type(&mut td.definition);
            }
            lex_ast::Stage::Import(_) => {}
        }
    }

    fn rewrite_expr(&mut self, e: &mut lex_ast::CExpr) {
        use lex_ast::CExpr::*;
        match e {
            Var { name } => *name = self.rename(name),
            Literal { .. } => {}
            Call { callee, args } => {
                self.rewrite_expr(callee);
                for a in args {
                    self.rewrite_expr(a);
                }
            }
            Let { value, body, ty, .. } => {
                if let Some(t) = ty {
                    self.rewrite_type(t);
                }
                self.rewrite_expr(value);
                self.rewrite_expr(body);
            }
            Match { scrutinee, arms } => {
                self.rewrite_expr(scrutinee);
                for arm in arms {
                    self.rewrite_expr(&mut arm.body);
                }
            }
            Block { statements, result } => {
                for s in statements {
                    self.rewrite_expr(s);
                }
                self.rewrite_expr(result);
            }
            Constructor { args, .. } => {
                for a in args {
                    self.rewrite_expr(a);
                }
            }
            RecordLit { fields } => {
                for f in fields {
                    self.rewrite_expr(&mut f.value);
                }
            }
            TupleLit { items } | ListLit { items } => {
                for i in items {
                    self.rewrite_expr(i);
                }
            }
            FieldAccess { value, .. } => self.rewrite_expr(value),
            Lambda { params, return_type, body, .. } => {
                for p in params {
                    self.rewrite_type(&mut p.ty);
                }
                self.rewrite_type(return_type);
                self.rewrite_expr(body);
            }
            BinOp { lhs, rhs, .. } => {
                self.rewrite_expr(lhs);
                self.rewrite_expr(rhs);
            }
            UnaryOp { expr, .. } => self.rewrite_expr(expr),
            Return { value } => self.rewrite_expr(value),
        }
    }

    fn rewrite_type(&mut self, t: &mut lex_ast::TypeExpr) {
        use lex_ast::TypeExpr::*;
        match t {
            Named { name, args } => {
                *name = self.rename(name);
                for a in args {
                    self.rewrite_type(a);
                }
            }
            Record { fields } => {
                for f in fields {
                    self.rewrite_type(&mut f.ty);
                }
            }
            Tuple { items } => {
                for i in items {
                    self.rewrite_type(i);
                }
            }
            Function { params, ret, .. } => {
                for p in params {
                    self.rewrite_type(p);
                }
                self.rewrite_type(ret);
            }
            Union { variants } => {
                for v in variants {
                    if let Some(pl) = &mut v.payload {
                        self.rewrite_type(pl);
                    }
                }
            }
            RecordWithSpreads { spreads, fields } => {
                for s in spreads {
                    *s = self.rename(s);
                }
                for f in fields {
                    self.rewrite_type(&mut f.ty);
                }
            }
            Refined { base, predicate, .. } => {
                self.rewrite_type(base);
                self.rewrite_expr(predicate);
            }
        }
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::is_mangle_prefix;

    #[test]
    fn recognizes_mangle_prefixes_not_stdlib_aliases() {
        // Inlined-dep / file mangle prefixes: <stem>_<hex6+>.
        assert!(is_mangle_prefix("lib_56ce0533"));
        assert!(is_mangle_prefix("schema_a1b2c3"));
        // Stdlib import aliases and ordinary names are not prefixes.
        assert!(!is_mangle_prefix("int"));
        assert!(!is_mangle_prefix("str"));
        assert!(!is_mangle_prefix("map_reduce")); // "reduce" isn't hex
        assert!(!is_mangle_prefix("nt"));
        assert!(!is_mangle_prefix("lib_xyz")); // too short / non-hex
    }
}
