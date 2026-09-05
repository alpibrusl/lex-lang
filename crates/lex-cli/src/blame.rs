//! `lex blame`: attestation-backed provenance for a function.

use super::*;

pub(super) fn cmd_blame(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // usage: lex blame [--store DIR] [--with-evidence] <file>
    let (root, mut rest, _, _) = parse_store_flag(args);
    let with_evidence = rest.iter().any(|a| a == "--with-evidence");
    rest.retain(|a| a != "--with-evidence");
    let path = rest
        .first()
        .ok_or_else(|| anyhow!("usage: lex blame [--store DIR] [--with-evidence] <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let store =
        Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    // Attestation log is opened once per blame run (not per stage)
    // so a 1000-entry blame doesn't pay 1000 fs::create_dir_all
    // calls. The log itself is just a path holder; reads are
    // per-stage directory listings.
    let att_log = if with_evidence {
        Some(store.attestation_log()?)
    } else {
        None
    };

    let mut entries = Vec::new();
    for s in &stages {
        // Imports don't have stage identities.
        if matches!(s, Stage::Import(_)) {
            continue;
        }
        let name = stage_name(s).to_string();
        let sig = match lex_ast::sig_id(s) {
            Some(id) => id,
            None => continue,
        };
        let here_stage = stage_id(s).unwrap_or_default();
        let history = store.sig_history(&sig)?;
        let active_stage = store.resolve_sig(&sig).ok().flatten();

        // Where does this source's stage stand?
        let here_status = history
            .iter()
            .find(|h| h.stage_id == here_stage)
            .map(|h| format!("{:?}", h.status).to_lowercase());

        let history_json: Vec<serde_json::Value> = history
            .iter()
            .map(|h| {
                let mut entry = serde_json::json!({
                    "stage_id": h.stage_id,
                    "status": format!("{:?}", h.status).to_lowercase(),
                    "last_at": h.last_at,
                    "published_at": h.published_at,
                });
                if let Some(log) = &att_log {
                    let mut atts = log.list_for_stage(&h.stage_id).unwrap_or_default();
                    atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert(
                            "attestations".into(),
                            serde_json::to_value(&atts).unwrap_or(serde_json::Value::Null),
                        );
                    }
                }
                entry
            })
            .collect();
        entries.push(serde_json::json!({
            "name": name,
            "sig_id": sig,
            "here_stage_id": here_stage,
            "here_status": here_status,    // None => unpublished
            "active_stage_id": active_stage,
            "history": history_json,
        }));

        // New: causal history from the op log.
        let log = lex_vcs::OpLog::open(store.root()).ok();
        let head_op = store
            .get_branch(&store.current_branch())
            .ok()
            .and_then(|opt| opt.and_then(|b| b.head_op));
        let causal: Vec<serde_json::Value> = match (log, head_op) {
            (Some(log), Some(head)) => {
                log.walk_back(&head, None)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| {
                        // Touch this sig (or, for renames, produce it as the new sig).
                        match &r.op.kind {
                            lex_vcs::OperationKind::AddFunction { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyBody { sig_id, .. }
                            | lex_vcs::OperationKind::ChangeEffectSig { sig_id, .. }
                            | lex_vcs::OperationKind::AddType { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyType { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveFunction { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveType { sig_id, .. } => sig_id == &sig,
                            lex_vcs::OperationKind::RenameSymbol { from, to, .. } => {
                                from == &sig || to == &sig
                            }
                            _ => false,
                        }
                    })
                    .map(|r| {
                        let kind_tag = serde_json::to_value(&r.op.kind)
                            .ok()
                            .and_then(|v| v.get("op").cloned())
                            .unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "op_id": r.op_id,
                            "kind": kind_tag,
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        // Mutate the most-recent entries.push value to attach causal_history.
        if let Some(last) = entries.last_mut() {
            last.as_object_mut()
                .unwrap()
                .insert("causal_history".into(), serde_json::Value::Array(causal));
        }
    }
    let data = serde_json::json!({ "blame": entries });
    let entries_for_text = entries.clone();
    acli::emit_or_text("blame", data, fmt, move || {
        for e in &entries_for_text {
            print_blame_entry(e);
        }
    });
    Ok(())
}

pub(super) fn print_blame_entry(e: &serde_json::Value) {
    let name = e["name"].as_str().unwrap_or("?");
    let sig = e["sig_id"].as_str().unwrap_or("");
    let here = e["here_stage_id"].as_str().unwrap_or("");
    let status = e["here_status"].as_str().unwrap_or("unpublished");
    let active = e["active_stage_id"].as_str();
    let history = e["history"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);

    println!("fn {name}");
    println!("  sig:     {sig:.16}…");
    if active.map(|a| a == here).unwrap_or(false) {
        println!("  current: {here:.16}…  ({status})");
    } else {
        println!("  current: {here:.16}…  ({status} in store)");
        if let Some(a) = active {
            println!("  active in store: {a:.16}…");
        }
    }
    if history.is_empty() {
        println!("  history: (not in store)");
    } else {
        println!("  history: {} stage(s)", history.len());
        for h in history {
            let sid = h["stage_id"].as_str().unwrap_or("");
            let st = h["status"].as_str().unwrap_or("?");
            let at = h["last_at"].as_u64().unwrap_or(0);
            let marker = if sid == here { " ←" } else { "" };
            println!("    {sid:.16}…  {st:<10} {}{marker}", format_blame_ts(at));
            // `--with-evidence` attaches attestations to each history
            // entry. Render compactly: one line per attestation,
            // showing kind, result, and producer.
            if let Some(atts) = h["attestations"].as_array() {
                if atts.is_empty() {
                    println!("      evidence: (none)");
                } else {
                    for a in atts {
                        let kind = a["kind"]["kind"].as_str().unwrap_or("?");
                        let result = a["result"]["result"].as_str().unwrap_or("?");
                        let tool = a["produced_by"]["tool"].as_str().unwrap_or("?");
                        let ver = a["produced_by"]["version"].as_str().unwrap_or("?");
                        println!("      {kind:<14} {result:<8} by {tool}@{ver}");
                    }
                }
            }
        }
    }
    println!();
}

pub(super) fn format_blame_ts(secs: u64) -> String {
    let mut s = secs as i64;
    let mut days = s.div_euclid(86_400);
    s = s.rem_euclid(86_400);
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    let mut y: i64 = 1970;
    loop {
        let yd = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            366
        } else {
            365
        };
        if days < yd {
            break;
        }
        days -= yd;
        y += 1;
    }
    let mdays = [
        31,
        if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
            29
        } else {
            28
        },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0usize;
    while mo < 12 && days >= mdays[mo] {
        days -= mdays[mo];
        mo += 1;
    }
    format!("{y:04}-{:02}-{:02}T{:02}:{:02}Z", mo + 1, days + 1, h, m)
}

pub(super) fn stage_name(s: &Stage) -> &str {
    match s {
        Stage::FnDecl(fd) => &fd.name,
        Stage::TypeDecl(td) => &td.name,
        Stage::Import(i) => &i.alias,
    }
}

/// Decode a CLI argument's JSON into a `Value`. Delegates to
/// `Value::from_json` so the CLI, the `lex serve` HTTP API, and
/// in-program `json.parse` all share the same convention — including
/// `{"$variant": "Name", "args": [...]}` for variants. (Closes #93.)
pub(super) fn json_to_value(v: &serde_json::Value) -> Value {
    Value::from_json(v)
}

/// Find the StageId of a function declared in `lex_src` whose name
/// matches `fn_name`. Returns `None` if the source doesn't parse,
/// the fn isn't there, or it's a non-FnDecl stage. Used by `lex
/// spec check` to tie its Spec attestation to the exact stage the
/// spec was verified against.
pub(super) fn find_stage_id_for_fn(lex_src: &str, fn_name: &str) -> Option<String> {
    let prog = load_program_from_str(lex_src).ok()?;
    let stages = canonicalize_program(&prog);
    let stage = stages
        .iter()
        .find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == fn_name))?;
    stage_id(stage)
}
