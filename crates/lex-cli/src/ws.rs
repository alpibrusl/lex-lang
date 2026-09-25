//! `lex ws` — the local-first workspace commands (#837 piece A): an embedded
//! store on disk, written to directly through the op log.
//!
//!   lex ws transform --branch B [--store DIR]
//!                    [--intent-prompt TEXT] [--intent-model P/N]
//!                    [--intent-session S] [--intent-issue ID]
//!                    <kind> --json '<params>'
//!
//! `<kind>` is one of `replace_match_arm | rename_local | inline_let |
//! extract_function`; `--json` carries that kind's params (the same object
//! `POST /v1/transform` takes under `transform`, minus `kind`, which the
//! positional supplies; `lex repair --transform` takes the same shape). It
//! calls [`lex_api::transform_http::apply_transform`] — the very function the
//! HTTP handler calls — so the two write doors cannot diverge: same gates,
//! same ops, same OpIds for the same input and intent.
//!
//! `--branch` is required. Like the HTTP surface this never falls back to the
//! store's "current branch", so an edit always says where it lands.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_api::transform_http::{apply_transform, IntentSpec, TransformSpec};
use lex_store::{Store, StoreError};
use std::path::PathBuf;

const USAGE: &str = "usage: lex ws transform --branch B [--store DIR] \
[--intent-prompt TEXT] [--intent-model PROVIDER/NAME] [--intent-session ID] [--intent-issue ISSUE_ID] \
<replace_match_arm|rename_local|inline_let|extract_function> --json '<params>'";

pub(crate) fn cmd_ws(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("transform") => cmd_ws_transform(fmt, &args[1..]),
        Some(other) => bail!("unknown `lex ws` subcommand `{other}`\n{USAGE}"),
        None => bail!("{USAGE}"),
    }
}

fn cmd_ws_transform(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut root: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut intent = IntentSpec::default();
    let mut params_json: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<String>, flag: &str| -> Result<String> {
        it.next()
            .cloned()
            .ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => root = Some(PathBuf::from(value(&mut it, "--store")?)),
            "--branch" => branch = Some(value(&mut it, "--branch")?),
            "--intent-prompt" => intent.prompt = Some(value(&mut it, "--intent-prompt")?),
            "--intent-model" => intent.model = Some(value(&mut it, "--intent-model")?),
            "--intent-session" => intent.session = Some(value(&mut it, "--intent-session")?),
            "--intent-issue" => intent.issue_id = Some(value(&mut it, "--intent-issue")?),
            "--json" => params_json = Some(value(&mut it, "--json")?),
            s if s.starts_with("--") => bail!("unknown flag `{s}`\n{USAGE}"),
            _ => positional.push(a.clone()),
        }
    }
    let [kind] = positional.as_slice() else {
        bail!("expected exactly one transform kind\n{USAGE}");
    };
    let branch = branch
        .filter(|b| !b.is_empty())
        .ok_or_else(|| anyhow!("--branch is required (the store's current branch is never assumed)\n{USAGE}"))?;
    let params_json = params_json.ok_or_else(|| anyhow!("--json '<params>' is required\n{USAGE}"))?;

    // params + the positional kind => the same tagged object the HTTP body carries.
    let mut params: serde_json::Value =
        serde_json::from_str(&params_json).with_context(|| format!("parsing --json: {params_json}"))?;
    let obj = params
        .as_object_mut()
        .ok_or_else(|| anyhow!("--json must be a JSON object of the transform's params"))?;
    match obj.get("kind") {
        Some(k) if k.as_str() != Some(kind.as_str()) => {
            bail!("--json `kind` ({k}) disagrees with the positional kind `{kind}`")
        }
        _ => {}
    }
    obj.insert("kind".into(), serde_json::Value::String(kind.clone()));
    let spec: TransformSpec = serde_json::from_value(params)
        .with_context(|| format!("invalid params for transform `{kind}`"))?;

    let intent = intent
        .into_intent(crate::store::default_intent_session)
        .map_err(|e| anyhow!(e))?;
    let root = root.unwrap_or_else(crate::default_store_root_pub);
    let store = Store::open(&root).map_err(|e| anyhow!("open store at {}: {e}", root.display()))?;

    match apply_transform(&store, &branch, &spec, &intent) {
        Ok(applied) => {
            let data = applied.to_json();
            acli::emit_or_text("ws", data, fmt, || {
                println!(
                    "{} applied on branch {}: op {} (head {})",
                    applied.kind,
                    applied.branch,
                    applied.op_ids.last().map(String::as_str).unwrap_or("?"),
                    applied.new_head.as_deref().unwrap_or("?"),
                );
            });
            Ok(())
        }
        // Same contract as `lex publish`: a write the gate refuses exits 2 with
        // the diagnostics; the branch head did not move.
        Err(StoreError::TypeError(errs)) => {
            let arr: Vec<serde_json::Value> =
                errs.iter().map(|e| serde_json::to_value(e).unwrap()).collect();
            let data = serde_json::json!({ "phase": "type-check", "errors": arr });
            acli::emit_or_text("ws", data, fmt, || {
                for e in &errs {
                    if let Ok(j) = serde_json::to_string(e) {
                        eprintln!("{j}");
                    }
                }
            });
            std::process::exit(2);
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}
