//! `lex export-git` — render a branch's op history as a git repository
//! (#837, Tier 3: the interop half of "source of record").
//!
//! The op log is the canonical history; git is a *view* of it. This
//! walks a branch oldest→newest, replays each op into a running
//! SigId→StageId head map, renders the resulting stages back to source
//! with the canonical printer, and lands one git commit per op — the
//! op's intent prompt as the message (falling back to a kind summary),
//! the op id as a trailer. The result is a repo humans, GitHub, and
//! IDEs can read, reconstructed deterministically from the typed log.
//!
//! Slice 1 renders the whole head to a single `src.lex` per commit.
//! Multi-file layout, a git-import path (text diff → typed ops via
//! `diff_to_ops`), and per-op author/date from the intent's model are
//! follow-ups on #837.
//!
//! Usage:
//!   lex export-git <out_dir> [--branch NAME] [--store DIR]

use super::*;
use lex_vcs::{default_import_alias, IntentLog, OpLog, OperationKind, StageTransition};
use std::collections::BTreeMap;
use std::process::Command;

pub fn cmd_export_git(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut out_dir: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--branch" => { branch = args.get(i + 1).cloned(); i += 2; }
            "--store" => { store_root = args.get(i + 1).map(PathBuf::from); i += 2; }
            other if !other.starts_with("--") && out_dir.is_none() => {
                out_dir = Some(PathBuf::from(other)); i += 1;
            }
            other => bail!("unexpected arg `{other}` (usage: lex export-git <out_dir> [--branch NAME] [--store DIR])"),
        }
    }
    let out_dir = out_dir.ok_or_else(|| anyhow!("usage: lex export-git <out_dir> [--branch NAME] [--store DIR]"))?;
    let root = store_root.unwrap_or_else(default_store_root);
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    let head = store
        .get_branch(&branch)?
        .and_then(|b| b.head_op);
    let log = OpLog::open(&root).with_context(|| "opening op log")?;
    let intents = IntentLog::open(&root).with_context(|| "opening intent log")?;

    let records = match &head {
        Some(h) => log.walk_forward(h, None)?,
        None => Vec::new(),
    };

    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    if !out_dir.join(".git").exists() {
        run_git(&out_dir, &["init", "-q"])?;
    }
    // Deterministic identity so re-exporting the same log is stable.
    run_git(&out_dir, &["config", "user.name", "lex-export"])?;
    run_git(&out_dir, &["config", "user.email", "lex-export@localhost"])?;

    let src_path = out_dir.join("src.lex");
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    // Imports live outside the SigId→StageId head map (they replay as
    // `ImportOnly`, a no-op there), so track them from the op kinds
    // directly. `flat_imports` is `reference` → `alias` for the
    // single-file render (#895); `file_imports` is the same per source
    // file, for the multi-file render (#894 slice 2b).
    let mut flat_imports: BTreeMap<String, String> = BTreeMap::new();
    let mut file_imports: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    // SigId → the source file its declaration came from (from each
    // AddFunction/AddType's `in_file`). When every head stage has one, the
    // package was published multi-module and we de-flatten it back into a
    // `src/*.lex` tree; otherwise we render one `src.lex`.
    let mut sig_files: BTreeMap<String, String> = BTreeMap::new();
    let mut commits = 0usize;

    for rec in &records {
        apply_transition(&mut map, &rec.produces);
        match &rec.op.kind {
            OperationKind::AddFunction { sig_id, in_file: Some(f), .. }
            | OperationKind::AddType { sig_id, in_file: Some(f), .. } => {
                sig_files.insert(sig_id.clone(), f.clone());
            }
            OperationKind::AddImport { in_file, module, alias } => {
                // The op omits the alias when it's the module's default;
                // reconstruct it the same way the store does.
                let alias = alias.clone().unwrap_or_else(|| default_import_alias(module));
                flat_imports.insert(module.clone(), alias.clone());
                file_imports.entry(in_file.clone()).or_default().insert(module.clone(), alias);
            }
            OperationKind::RemoveImport { in_file, module } => {
                flat_imports.remove(module);
                if let Some(m) = file_imports.get_mut(in_file) {
                    m.remove(module);
                }
            }
            OperationKind::RenameSymbol { from, to, .. } => {
                if let Some(f) = sig_files.remove(from) {
                    sig_files.insert(to.clone(), f);
                }
            }
            _ => {}
        }

        // Start each commit from a clean tree so a stage moving files, or a
        // file emptying out, is reflected (git add -A then picks up the net
        // change). Cheap relative to the op replay itself.
        let _ = std::fs::remove_file(&src_path);
        let _ = std::fs::remove_dir_all(out_dir.join("src"));

        // Multi-file iff every head fn/type stage records its source file.
        if !map.is_empty() && map.keys().all(|sig| sig_files.contains_key(sig)) {
            render_multifile(&out_dir, &store, &map, &sig_files, &file_imports)?;
        } else {
            render_singlefile(&src_path, &store, &map, &flat_imports)?;
        }

        // Commit message: the intent prompt, else a kind summary.
        let msg = commit_message(&intents, rec)?;
        run_git(&out_dir, &["add", "-A"])?;
        // --allow-empty: an ImportOnly op (or a no-op transition)
        // doesn't change the tree, but the commit still records the op.
        run_git(&out_dir, &["commit", "-q", "--allow-empty", "-m", &msg])?;
        commits += 1;
    }

    let data = serde_json::json!({
        "out_dir": out_dir.display().to_string(),
        "branch": branch,
        "commits": commits,
    });
    let out_for_text = out_dir.display().to_string();
    let branch_for_text = branch.clone();
    acli::emit_or_text("export-git", data, fmt, move || {
        println!("exported {commits} commit(s) from branch {branch_for_text} to {out_for_text}");
    });
    Ok(())
}

/// Render the whole head as one `src.lex` (single file / single module).
fn render_singlefile(
    src_path: &std::path::Path,
    store: &Store,
    map: &BTreeMap<String, String>,
    imports: &BTreeMap<String, String>,
) -> Result<()> {
    let mut stages: Vec<lex_ast::Stage> = Vec::with_capacity(imports.len() + map.len());
    for (reference, alias) in imports {
        stages.push(lex_ast::Stage::Import(lex_ast::Import {
            reference: reference.clone(),
            alias: alias.clone(),
        }));
    }
    // Read per-SigId (not by StageId) so structurally identical stages
    // that share a StageId keep their distinct names (see render_multifile).
    let head_pairs: Vec<(String, String)> =
        map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    for ast in store.get_asts_for_sigs_bulk(&head_pairs) {
        stages.push(ast?);
    }
    let source = lex_ast::print_stages(&stages);
    std::fs::write(src_path, source).with_context(|| format!("writing {}", src_path.display()))?;
    Ok(())
}

/// De-flatten a mangled multi-module package back into its `src/*.lex`
/// tree (#894 slice 2b). Each declaration's mangled name is
/// `<prefix>.<local>`; `sig_files` says which file each belongs to. Group
/// by file, strip each file's own prefix, and rewrite a reference to
/// another file's prefix into `alias.name` plus a local
/// `import "./<relpath>" as alias`.
///
/// Faithful except for one thing the op log cannot recover: a *local*
/// import's non-default alias. `import "./error" as e` was flattened away
/// at publish (the reference became `error_<hash>.…`), so it comes back as
/// `import "./error" as error` — functionally identical and compiling.
/// Stdlib aliases are preserved (they ride on `AddImport`).
fn render_multifile(
    out_dir: &std::path::Path,
    store: &Store,
    map: &BTreeMap<String, String>,
    sig_files: &BTreeMap<String, String>,
    file_imports: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<()> {
    let mut prefix_to_file: BTreeMap<String, String> = BTreeMap::new();
    let mut by_file: BTreeMap<String, Vec<lex_ast::Stage>> = BTreeMap::new();
    // Read each stage through the SigId the head names it by — NOT by
    // StageId. A StageId is name-independent, so two structurally identical
    // helpers across modules (`contains_str` in both `schema_import.lex`
    // and `constraints.lex`) share one; reading by StageId returns a single
    // name for the pair and would file one under the other's prefix
    // (#818/#894). `get_asts_for_sigs_bulk` recovers the right name per sig.
    let head_pairs: Vec<(String, String)> =
        map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let asts = store.get_asts_for_sigs_bulk(&head_pairs);
    for ((sig, _), ast) in head_pairs.iter().zip(asts) {
        let stage = ast?;
        let file = sig_files.get(sig).cloned().unwrap_or_default();
        if let Some(prefix) = stage_prefix(&stage) {
            prefix_to_file.insert(prefix, file.clone());
        }
        by_file.entry(file).or_default().push(stage);
    }

    for (file, stages) in &by_file {
        // Every stage in a file shares its prefix.
        let own_prefix = stages.iter().find_map(stage_prefix).unwrap_or_default();
        // Names bound as params / lets / lambda-params / patterns in this
        // file. A local import's alias must not be one of them, or it would
        // be shadowed (`import "./field" as field` under `field :: Str` —
        // lex-schema used `as f` to dodge this, but that alias was
        // flattened away). When the stem collides we fall back to the
        // guaranteed-unique mangle prefix, which no bare local can be.
        let mut bound_locals = std::collections::BTreeSet::new();
        for s in stages {
            collect_bound_locals(s, &mut bound_locals);
        }
        let mut rw = FileRewrite {
            own_prefix: &own_prefix,
            own_file: file,
            prefix_to_file: &prefix_to_file,
            bound_locals: &bound_locals,
            local_imports: BTreeMap::new(),
        };
        let rewritten: Vec<lex_ast::Stage> = stages
            .iter()
            .cloned()
            .map(|mut s| {
                rw.rewrite_stage(&mut s);
                s
            })
            .collect();

        // Stdlib imports for this file + the local imports the rewrite found.
        let mut imports: BTreeMap<String, String> = file_imports.get(file).cloned().unwrap_or_default();
        imports.extend(rw.local_imports);

        let mut out_stages: Vec<lex_ast::Stage> = Vec::new();
        for (reference, alias) in &imports {
            out_stages.push(lex_ast::Stage::Import(lex_ast::Import {
                reference: reference.clone(),
                alias: alias.clone(),
            }));
        }
        out_stages.extend(rewritten);

        let source = lex_ast::print_stages(&out_stages);
        let path = out_dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, source).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
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
    while i < from_dir.len()
        && i + 1 < to_parts.len()
        && from_dir[i] == to_parts[i]
    {
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

/// Rewrites the mangled names in one file's stages back to source form.
/// Bare names bound in a stage (params, lets, lambda params, match
/// bindings) — the names an import alias must avoid to not be shadowed.
fn collect_bound_locals(s: &lex_ast::Stage, out: &mut std::collections::BTreeSet<String>) {
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

fn collect_expr_locals(e: &lex_ast::CExpr, out: &mut std::collections::BTreeSet<String>) {
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

fn collect_pattern_locals(p: &lex_ast::Pattern, out: &mut std::collections::BTreeSet<String>) {
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
    bound_locals: &'a std::collections::BTreeSet<String>,
    /// Local imports discovered while rewriting: import ref → alias.
    local_imports: BTreeMap<String, String>,
}

impl FileRewrite<'_> {
    /// Un-mangle a dotted name for THIS file: own prefix → bare; another
    /// package file's prefix → `alias.rest` (recording the import);
    /// anything else (a stdlib alias like `int.to_str`, or a bare name) is
    /// left untouched.
    fn rename(&mut self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix(&format!("{}.", self.own_prefix)) {
            return rest.to_string();
        }
        if let Some((q, rest)) = name.split_once('.') {
            if q != self.own_prefix {
                if let Some(other_file) = self.prefix_to_file.get(q) {
                    let (import_ref, stem) = relative_import(self.own_file, other_file);
                    // A stem that shadows a local binding is unusable as an
                    // alias — fall back to the mangle prefix, which no bare
                    // local can equal.
                    let alias = if self.bound_locals.contains(&stem) {
                        q.to_string()
                    } else {
                        stem
                    };
                    self.local_imports.insert(import_ref, alias.clone());
                    return format!("{alias}.{rest}");
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
            // Constructor names are global (never mangled); rewrite args only.
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

/// One op → one commit message. The intent's prompt is the actual
/// causal event (a commit message can be made up; the prompt is what
/// happened), so prefer it; fall back to the op kind. The op id goes in
/// a trailer so the git view is traceable back to the log.
fn commit_message(intents: &IntentLog, rec: &lex_vcs::OperationRecord) -> Result<String> {
    let subject = match &rec.op.intent_id {
        Some(id) => intents
            .get(id)?
            .map(|i| first_line(&i.prompt))
            .unwrap_or_else(|| kind_summary(&rec.op.kind)),
        None => kind_summary(&rec.op.kind),
    };
    let intent_line = rec
        .op
        .intent_id
        .as_ref()
        .map(|id| format!("\nIntent: {id}"))
        .unwrap_or_default();
    Ok(format!("{subject}\n\nOp: {}{intent_line}", rec.op_id))
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.is_empty() { "(empty prompt)".to_string() } else { line.to_string() }
}

fn kind_summary(kind: &lex_vcs::OperationKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.get("op").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_else(|| "op".to_string())
}

/// Replay a transition into the running head map. Mirrors
/// `lex_store`'s own private `apply_transition`; kept in sync with the
/// `StageTransition` variants (the enum is `#[non_exhaustive]`-free, so
/// the compiler flags a new variant here).
fn apply_transition(map: &mut BTreeMap<String, String>, t: &StageTransition) {
    match t {
        StageTransition::Create { sig_id, stage_id }
        | StageTransition::Replace { sig_id, to: stage_id, .. } => {
            map.insert(sig_id.clone(), stage_id.clone());
        }
        StageTransition::Remove { sig_id, .. } => {
            map.remove(sig_id);
        }
        StageTransition::Rename { from, to, body_stage_id } => {
            map.remove(from);
            map.insert(to.clone(), body_stage_id.clone());
        }
        StageTransition::ImportOnly => {}
        StageTransition::Merge { entries } => {
            for (sig, stage) in entries {
                match stage {
                    Some(s) => { map.insert(sig.clone(), s.clone()); }
                    None => { map.remove(sig); }
                }
            }
        }
    }
}

fn run_git(dir: &std::path::Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}
