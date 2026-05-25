//! `lex docs --for-agent` — structured machine-readable workspace
//! context (#282).
//!
//! When an agent loads into a Lex repository, it has no single
//! place to read "what's in this workspace right now." It has to
//! grep `stages/`, parse `policy.json`, list branches, walk
//! intents — every agent reinvents the discovery glue and burns
//! tokens re-learning the same shape every session.
//!
//! `lex docs --for-agent` emits one JSON object structured for
//! prompt consumption. All sections are derivable from existing
//! on-disk state; no new persistence.

use crate::acli;
use crate::fmt::collect_lex_files;
use ::acli::OutputFormat;
use anyhow::{anyhow, Context, Result};
use lex_store::{Store, DEFAULT_BRANCH};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Schema version of the emitted JSON. Bump when fields are
/// removed or semantically reshaped; adding optional fields is
/// schema-additive and doesn't require a bump.
const LEX_DOCS_VERSION: u32 = 1;

pub fn cmd_docs(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex docs <path>...        # API docs for source files/dirs\n\
         usage: lex docs --for-agent [--branch B] [--limit-recent N] [--store DIR]\n\
         usage: lex docs --rules"))?;
    if sub == "--rules" {
        // #306 slice 2: enumerate every type-error rule with its
        // explanation. Stable kebab-case `rule_tag`s let LLM repair
        // flows cross-reference rules across runs.
        let rules: Vec<serde_json::Value> = lex_types::all_rules()
            .iter()
            .map(|r| serde_json::json!({
                "rule_tag": r.tag,
                "rule_explanation": r.explanation,
            }))
            .collect();
        let data = serde_json::json!({ "rules": rules });
        acli::emit_or_text("docs", data, fmt, || {
            println!("Lex type-error rules ({} total):", lex_types::all_rules().len());
            for r in lex_types::all_rules() {
                println!();
                println!("  {}", r.tag);
                println!("    {}", r.explanation);
            }
        });
        return Ok(());
    }
    if sub.starts_with("--") && sub != "--for-agent" {
        anyhow::bail!(
            "unknown `lex docs` flag `{sub}`. supported: `lex docs <path>...`, `--for-agent`, `--rules`"
        );
    }
    if sub != "--for-agent" {
        // No recognized flag → treat every arg as a source file/dir and
        // emit API docs (#564).
        return cmd_docs_source(fmt, args);
    }
    let mut branch: Option<String> = None;
    let mut limit_recent: usize = 50;
    let mut root: Option<PathBuf> = None;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next()
                    .ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
            }
            "--limit-recent" => {
                limit_recent = it.next()
                    .ok_or_else(|| anyhow!("--limit-recent needs N"))?
                    .parse()
                    .map_err(|e| anyhow!("--limit-recent: {e}"))?;
            }
            "--store" => {
                root = Some(PathBuf::from(it.next()
                    .ok_or_else(|| anyhow!("--store needs a path"))?));
            }
            other => anyhow::bail!("unexpected arg `{other}`"),
        }
    }
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    let store = Store::open(&root)?;
    let current_branch = branch.unwrap_or_else(|| store.current_branch());

    let envelope = build_envelope(&store, &current_branch, limit_recent)?;
    let data = serde_json::to_value(&envelope)?;
    acli::emit_or_text("docs", data, fmt, || render_text(&envelope));
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct DocsEnvelope {
    pub lex_docs_version: u32,
    pub workspace: Workspace,
    pub stdlib: StdlibSummary,
    pub recent_activity: Vec<RecentOp>,
    pub open_intents: Vec<OpenIntent>,
    pub policy: PolicySummary,
    pub attention: Vec<AttentionItem>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub lex_version: String,
    pub current_branch: String,
    pub default_branch: String,
    pub branches: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct StdlibSummary {
    /// Every active sig on `current_branch`. The name is the
    /// function name (stable across body edits); `type_signature`
    /// is a human-readable render of `(params) -> ret [effects]`.
    pub sigs: Vec<SigInfo>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SigInfo {
    pub sig_id: String,
    pub stage_id: String,
    pub name: String,
    pub type_signature: String,
    pub effects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct RecentOp {
    pub op_id: String,
    pub kind_tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct OpenIntent {
    pub intent_id: String,
    pub prompt: String,
    pub session_id: String,
    pub model: serde_json::Value,
    /// Op ids on `current_branch` that reference this intent.
    pub produced_ops: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PolicySummary {
    pub required_attestations: serde_json::Value,
    pub blocked_producers: serde_json::Value,
    pub gc_retention: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct AttentionItem {
    pub stage_id: String,
    pub sig_id: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn build_envelope(
    store: &Store,
    branch: &str,
    limit_recent: usize,
) -> Result<DocsEnvelope> {
    let branches = store.list_branches()?;
    let workspace = Workspace {
        lex_version: env!("CARGO_PKG_VERSION").into(),
        current_branch: branch.into(),
        default_branch: DEFAULT_BRANCH.into(),
        branches,
    };

    let head = store.branch_head(branch).unwrap_or_default();
    let mut sigs: Vec<SigInfo> = Vec::with_capacity(head.len());
    for (sig_id, stage_id) in &head {
        let Ok(ast) = store.get_ast(stage_id) else { continue };
        let lex_ast::Stage::FnDecl(fd) = ast else { continue };
        let effects: Vec<String> = fd.effects.iter()
            .map(|e| match &e.arg {
                Some(lex_ast::EffectArg::Int { value }) => format!("{}({})", e.name, value),
                Some(lex_ast::EffectArg::Str { value }) => format!("{}({:?})", e.name, value),
                Some(lex_ast::EffectArg::Ident { value }) => format!("{}({})", e.name, value),
                None => e.name.clone(),
            })
            .collect();
        let budget = fd.effects.iter()
            .filter_map(|e| if e.name == "budget" {
                match &e.arg {
                    Some(lex_ast::EffectArg::Int { value }) => Some(*value as u64),
                    _ => None,
                }
            } else {
                None
            })
            .min();
        sigs.push(SigInfo {
            sig_id: sig_id.clone(),
            stage_id: stage_id.clone(),
            name: fd.name.clone(),
            type_signature: render_signature(&fd),
            effects,
            budget,
        });
    }
    sigs.sort_by(|a, b| a.name.cmp(&b.name));

    let recent_activity = collect_recent_activity(store, branch, limit_recent)?;
    let open_intents = collect_open_intents(store, &recent_activity)?;
    let policy = load_policy_summary(store)?;
    let attention = collect_attention(store, &head)?;

    Ok(DocsEnvelope {
        lex_docs_version: LEX_DOCS_VERSION,
        workspace,
        stdlib: StdlibSummary { sigs },
        recent_activity,
        open_intents,
        policy,
        attention,
    })
}

fn collect_recent_activity(
    store: &Store,
    branch: &str,
    limit: usize,
) -> Result<Vec<RecentOp>> {
    let head_op = match store.get_branch(branch)? {
        Some(b) => b.head_op,
        None => return Ok(Vec::new()),
    };
    let Some(head) = head_op else { return Ok(Vec::new()); };
    let log = lex_vcs::OpLog::open(store.root())?;
    // `walk_back(_, Some(0))` still returns one record (the limit
    // check fires after the push). Pass `None` and truncate
    // explicitly so `--limit-recent 0` actually yields zero.
    let mut walked = log.walk_back(&head, None)?;
    walked.truncate(limit);
    let mut out: Vec<RecentOp> = Vec::with_capacity(walked.len());
    for rec in walked {
        let kind_tag = op_kind_tag(&rec.op.kind);
        let (sig_id, stage_id) = match rec.op.kind.merge_target() {
            Some((s, st)) => (Some(s), st),
            None => (None, None),
        };
        out.push(RecentOp {
            op_id: rec.op_id,
            kind_tag: kind_tag.into(),
            sig_id,
            stage_id,
            intent_id: rec.op.intent_id,
        });
    }
    Ok(out)
}

fn op_kind_tag(k: &lex_vcs::OperationKind) -> &'static str {
    use lex_vcs::OperationKind::*;
    match k {
        AddFunction { .. }     => "add_function",
        RemoveFunction { .. }  => "remove_function",
        ModifyBody { .. }      => "modify_body",
        RenameSymbol { .. }    => "rename_symbol",
        ChangeEffectSig { .. } => "change_effect_sig",
        AddImport { .. }       => "add_import",
        RemoveImport { .. }    => "remove_import",
        AddType { .. }         => "add_type",
        RemoveType { .. }      => "remove_type",
        ModifyType { .. }      => "modify_type",
        Merge { .. }           => "merge",
        ReplaceMatchArm { .. } => "replace_match_arm",
        RenameLocal { .. }     => "rename_local",
        InlineLet { .. }       => "inline_let",
        Candidate { .. }       => "candidate",
        Promote { .. }         => "promote",
    }
}

fn collect_open_intents(
    store: &Store,
    recent: &[RecentOp],
) -> Result<Vec<OpenIntent>> {
    use std::collections::BTreeMap;
    let log = lex_vcs::IntentLog::open(store.root())?;
    let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for op in recent {
        if let Some(id) = &op.intent_id {
            buckets.entry(id.clone()).or_default().push(op.op_id.clone());
        }
    }
    let mut out: Vec<OpenIntent> = Vec::with_capacity(buckets.len());
    for (intent_id, ops) in buckets {
        let Some(intent) = log.get(&intent_id)? else { continue };
        out.push(OpenIntent {
            intent_id,
            prompt: intent.prompt,
            session_id: intent.session_id,
            model: serde_json::to_value(&intent.model)
                .unwrap_or(serde_json::Value::Null),
            produced_ops: ops,
        });
    }
    Ok(out)
}

fn load_policy_summary(store: &Store) -> Result<PolicySummary> {
    let policy = lex_store::policy::load(store.root())?.unwrap_or_default();
    Ok(PolicySummary {
        required_attestations: serde_json::to_value(&policy.required_attestations)?,
        blocked_producers: serde_json::to_value(&policy.blocked_producers)?,
        gc_retention: serde_json::to_value(&policy.gc_retention)?,
    })
}

fn collect_attention(
    store: &Store,
    head: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<AttentionItem>> {
    // A stage warrants attention when it's currently active *and*
    // it carries an attestation that blocks it: a `Block { … }`
    // not subsequently `Unblock`'d, or a `ProducerBlock { tool }`
    // whose attestation is referenced from this stage's TypeCheck
    // chain. The negative path is what reviewers / agents should
    // see first.
    let attlog = store.attestation_log()?;
    let mut out: Vec<AttentionItem> = Vec::new();
    for (sig_id, stage_id) in head {
        let atts = match attlog.list_for_stage(stage_id) {
            Ok(a) => a,
            Err(_) => continue,
        };
        if lex_vcs::is_stage_blocked(&atts) {
            out.push(AttentionItem {
                stage_id: stage_id.clone(),
                sig_id: sig_id.clone(),
                reason: "blocked_by_attestation".into(),
                detail: Some("a `Block` attestation has not been subsequently `Unblock`'d".into()),
            });
        }
    }
    Ok(out)
}

fn render_signature(fd: &lex_ast::FnDecl) -> String {
    let params: Vec<String> = fd.params.iter()
        .map(|p| format!("{} :: {}", p.name, render_type(&p.ty))).collect();
    let effects = if fd.effects.is_empty() {
        String::new()
    } else {
        let s: Vec<String> = fd.effects.iter().map(|e| e.name.clone()).collect();
        format!(" [{}]", s.join(", "))
    };
    format!("({}) -> {}{}", params.join(", "), render_type(&fd.return_type), effects)
}

fn render_type(t: &lex_ast::TypeExpr) -> String {
    use lex_ast::TypeExpr::*;
    match t {
        Named { name, args } if args.is_empty() => name.clone(),
        Named { name, args } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{}<{}>", name, inner.join(", "))
        }
        Tuple { items } => {
            let inner: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", inner.join(", "))
        }
        Record { fields } => {
            let inner: Vec<String> = fields.iter()
                .map(|f| format!("{}: {}", f.name, render_type(&f.ty))).collect();
            format!("{{{}}}", inner.join(", "))
        }
        Function { params, ret, .. } => {
            let inner: Vec<String> = params.iter().map(render_type).collect();
            format!("({}) -> {}", inner.join(", "), render_type(ret))
        }
        Union { variants } => {
            let inner: Vec<String> = variants.iter().map(|v| v.name.clone()).collect();
            inner.join(" | ")
        }
        Refined { base, .. } => format!("{}{{…}}", render_type(base)),
        RecordWithSpreads { spreads, fields } => {
            let mut parts: Vec<String> = spreads.iter().map(|s| format!("...{}", s)).collect();
            parts.extend(fields.iter().map(|f| format!("{}: {}", f.name, render_type(&f.ty))));
            format!("{{{}}}", parts.join(", "))
        }
    }
}

fn render_text(env: &DocsEnvelope) {
    println!("Lex workspace docs (v{})", env.lex_docs_version);
    println!("  lex {} on branch `{}` (default: `{}`)",
        env.workspace.lex_version, env.workspace.current_branch,
        env.workspace.default_branch);
    println!("  branches: {}", env.workspace.branches.join(", "));
    println!();
    println!("Stdlib ({} sigs):", env.stdlib.sigs.len());
    for s in &env.stdlib.sigs {
        println!("  {}{}", s.name, s.type_signature);
    }
    println!();
    println!("Recent activity ({} ops):", env.recent_activity.len());
    for op in &env.recent_activity {
        let sig = op.sig_id.as_deref().unwrap_or("-");
        println!("  {} [{}] {}", &op.op_id[..12.min(op.op_id.len())], op.kind_tag, sig);
    }
    println!();
    println!("Open intents: {}", env.open_intents.len());
    for i in &env.open_intents {
        println!("  {} \"{}\" ({} ops)",
            &i.intent_id[..12.min(i.intent_id.len())], i.prompt, i.produced_ops.len());
    }
    println!();
    if !env.attention.is_empty() {
        println!("Attention queue: {} item(s)", env.attention.len());
        for a in &env.attention {
            println!("  {} {} — {}", a.sig_id, a.stage_id, a.reason);
        }
    }
}

// --- #564: `lex docs <path>` — API docs from source ----------------

/// Schema version of the `lex docs <path>` API-docs JSON. Bump on a
/// breaking reshape; adding optional fields is schema-additive.
const LEX_API_DOCS_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ApiDocs {
    pub lex_docs_version: u32,
    /// Package name from the nearest `lex.toml`, if one is found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Package version from the nearest `lex.toml` (empty when absent).
    pub version: String,
    pub modules: Vec<ModuleDoc>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ModuleDoc {
    /// Path to the source file, as supplied on the command line.
    pub file: String,
    /// Module-level doc: the `#` comment block at the top of the file
    /// (before the first item). The parser attributes top-of-file
    /// comments to the module rather than to the first declaration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub functions: Vec<FnDoc>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FnDoc {
    pub name: String,
    /// Stable SigId (canonical-AST hash). Absent only if the stage
    /// can't produce one (e.g. malformed signature).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig_id: Option<String>,
    /// Human-readable `(params) -> ret [effects]` render.
    pub signature: String,
    pub effects: Vec<String>,
    /// `examples {}` cases rendered as `name(args) => expected`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// Doc comment (the `#` lines immediately preceding the fn), with
    /// the leading `#` stripped and lines joined by newlines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

fn cmd_docs_source(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let paths: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();
    let files = collect_lex_files(&paths)?;
    if files.is_empty() {
        anyhow::bail!("no .lex files found under {:?}", args);
    }

    let (package, version) = package_meta(&paths);

    let mut modules = Vec::with_capacity(files.len());
    for file in &files {
        let src = std::fs::read_to_string(file)
            .with_context(|| format!("reading {}", file.display()))?;
        let prog = lex_syntax::parse_source(&src)
            .map_err(|e| anyhow!("parse error in {}: {e:?}", file.display()))?;

        // Doc comments live on the syntax tree (the canonicalizer strips
        // them so they never enter the SigId). Map fn name → doc here,
        // then look them up while walking the canonical stages.
        let mut docs_by_name: BTreeMap<String, String> = BTreeMap::new();
        for item in &prog.items {
            if let lex_syntax::Item::FnDecl(fd) = item {
                if let Some(doc) = clean_doc(&fd.leading_comments) {
                    docs_by_name.insert(fd.name.clone(), doc);
                }
            }
        }

        let stages = lex_ast::canonicalize_program(&prog);
        let mut functions = Vec::new();
        for stage in &stages {
            let lex_ast::Stage::FnDecl(fd) = stage else { continue };
            let examples = fd.examples.iter()
                .map(|ex| lex_ast::print_example(&fd.name, ex))
                .collect();
            functions.push(FnDoc {
                name: fd.name.clone(),
                sig_id: lex_ast::sig_id(stage),
                signature: render_signature(fd),
                effects: effect_strings(fd),
                examples,
                doc: docs_by_name.get(&fd.name).cloned(),
            });
        }

        modules.push(ModuleDoc {
            file: file.display().to_string(),
            doc: clean_doc(&prog.leading_comments),
            functions,
        });
    }

    let docs = ApiDocs {
        lex_docs_version: LEX_API_DOCS_VERSION,
        package,
        version,
        modules,
    };
    let data = serde_json::to_value(&docs)?;
    acli::emit_or_text("docs", data, fmt, || render_api_text(&docs));
    Ok(())
}

/// Resolve `(package_name, package_version)` from the nearest `lex.toml`
/// above the first supplied path. Missing manifest or `[package]` table
/// yields `(None, "")` — docs generation doesn't require a manifest.
fn package_meta(paths: &[PathBuf]) -> (Option<String>, String) {
    let start = paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let Some((toml_path, _)) = lex_syntax::find_manifest(&start) else {
        return (None, String::new());
    };
    match lex_syntax::Manifest::load(&toml_path) {
        Ok(m) => match m.package {
            Some(p) => (Some(p.name), p.version),
            None => (None, String::new()),
        },
        Err(_) => (None, String::new()),
    }
}

/// Strip the leading `#` (and one optional space) from each comment line
/// and join them. Returns `None` when there are no comment lines.
fn clean_doc(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let cleaned: Vec<String> = lines.iter()
        .map(|l| {
            let s = l.trim_start().trim_start_matches('#');
            s.strip_prefix(' ').unwrap_or(s).to_string()
        })
        .collect();
    Some(cleaned.join("\n"))
}

/// Render a canonical fn's effect row as a list of strings, mirroring the
/// parameterized-effect rendering used by the `--for-agent` envelope.
fn effect_strings(fd: &lex_ast::FnDecl) -> Vec<String> {
    fd.effects.iter()
        .map(|e| match &e.arg {
            Some(lex_ast::EffectArg::Int { value }) => format!("{}({})", e.name, value),
            Some(lex_ast::EffectArg::Str { value }) => format!("{}({:?})", e.name, value),
            Some(lex_ast::EffectArg::Ident { value }) => format!("{}({})", e.name, value),
            None => e.name.clone(),
        })
        .collect()
}

fn render_api_text(docs: &ApiDocs) {
    let pkg = docs.package.as_deref().unwrap_or("(no package)");
    println!("API docs for {pkg} {} (schema v{})", docs.version, docs.lex_docs_version);
    for m in &docs.modules {
        println!();
        println!("{} ({} fn):", m.file, m.functions.len());
        if let Some(doc) = &m.doc {
            for line in doc.lines() {
                println!("  # {line}");
            }
        }
        for f in &m.functions {
            let sid = f.sig_id.as_deref().map(|s| &s[..12.min(s.len())]).unwrap_or("-");
            println!("  {}{}  [{}]", f.name, f.signature, sid);
            if let Some(doc) = &f.doc {
                for line in doc.lines() {
                    println!("    {line}");
                }
            }
            for ex in &f.examples {
                println!("    e.g. {ex}");
            }
        }
    }
}
