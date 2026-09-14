//! `lex recall` — predicate queries over the op log (#836 G2).
//!
//! The op log + intents form a memory of *what* happened and *why*;
//! before this, the only query surface was `lex op log` (ancestry of
//! one branch head) and `lex blame` (one sig). `recall` answers the
//! cross-cutting questions the README lists as the motivation for the
//! op log — "everything agent X did under intent Y", "everything in
//! session Z" — by evaluating a [`Predicate`] over the whole log.
//!
//! Usage:
//!   lex recall --intent <intent_id>     [--store DIR] [--limit N]
//!   lex recall --session <session_id>   [--store DIR] [--limit N]
//!   lex recall --predicate '<json>'     [--store DIR] [--limit N]
//!   lex recall --all                    [--store DIR] [--limit N]
//!
//! `--session` resolves the session→intent mapping through the store's
//! `IntentLog` (a `Session` predicate matches an op iff its intent's
//! recorded `session_id` matches).

use super::*;
use lex_vcs::{IntentId, IntentLog, OpLog, Predicate, SessionId};

/// Store-backed [`lex_vcs::IntentResolver`]: an op's session is its
/// recorded intent's `session_id`, looked up in the `IntentLog`.
struct IntentLogResolver {
    log: IntentLog,
}

impl lex_vcs::IntentResolver for IntentLogResolver {
    fn session_of(&self, intent_id: &IntentId) -> Option<SessionId> {
        self.log.get(intent_id).ok().flatten().map(|i| i.session_id)
    }
}

pub fn cmd_recall(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut intent: Option<String> = None;
    let mut session: Option<String> = None;
    let mut predicate_json: Option<String> = None;
    let mut all = false;
    let mut limit: Option<usize> = None;
    let mut store_root: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--intent" => { intent = args.get(i + 1).cloned(); i += 2; }
            "--session" => { session = args.get(i + 1).cloned(); i += 2; }
            "--predicate" => { predicate_json = args.get(i + 1).cloned(); i += 2; }
            "--all" => { all = true; i += 1; }
            "--limit" => {
                limit = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--store" => { store_root = args.get(i + 1).map(PathBuf::from); i += 2; }
            other => bail!(
                "unexpected arg `{other}` (usage: lex recall \
                 [--intent ID | --session ID | --predicate JSON | --all] [--limit N] [--store DIR])"
            ),
        }
    }

    // Exactly one selector.
    let selectors = [intent.is_some(), session.is_some(), predicate_json.is_some(), all]
        .iter().filter(|b| **b).count();
    if selectors != 1 {
        bail!("choose exactly one of --intent, --session, --predicate, --all");
    }

    let predicate = if let Some(id) = intent {
        Predicate::Intent { intent_id: id }
    } else if let Some(id) = session {
        Predicate::Session { session_id: id }
    } else if let Some(json) = predicate_json {
        let v: serde_json::Value = serde_json::from_str(&json)
            .with_context(|| "parsing --predicate JSON")?;
        Predicate::from_value(&v).map_err(|e| anyhow!("bad predicate: {e}"))?
    } else {
        Predicate::All
    };

    let root = store_root.unwrap_or_else(default_store_root);
    let log = OpLog::open(&root).with_context(|| format!("opening op log at {}", root.display()))?;
    let resolver = IntentLogResolver {
        log: IntentLog::open(&root).with_context(|| "opening intent log")?,
    };
    let mut recs = lex_vcs::evaluate_with_resolver(&log, &predicate, &resolver)
        .with_context(|| "evaluating predicate")?;
    if let Some(n) = limit {
        recs.truncate(n);
    }

    let arr: Vec<serde_json::Value> = recs.iter()
        .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
        .collect();
    let count = arr.len();
    let data = serde_json::json!({ "count": count, "ops": arr });
    acli::emit_or_text("recall", data, fmt, move || {
        println!("{count} op(s)");
        for r in &recs {
            // Compact: op_id + kind tag.
            let tag = serde_json::to_value(&r.op.kind).ok()
                .and_then(|v| v.get("op").and_then(|s| s.as_str()).map(String::from))
                .unwrap_or_else(|| "?".into());
            println!("  {}  {}", r.op_id, tag);
        }
    });
    Ok(())
}
