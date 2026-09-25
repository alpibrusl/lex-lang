//! The incremental-import watermark, derived from the op-log itself.
//!
//! Nothing local records "how far we got": blobrefs are local and mutable, and
//! only locks sync. The op-log does sync, and every imported op carries an
//! intent whose `origin.commit` names the git commit it came from — so the
//! watermark is "walk back from the branch head to the nearest op whose intent
//! has an origin". It survives `op push`/`op pull` for free, and two clones of
//! the store agree on it.

use anyhow::Result;
use lex_store::Store;
use lex_vcs::{IntentLog, OpLog};
use std::collections::BTreeSet;

/// The last imported commit on a store branch.
#[derive(Debug, Clone)]
pub(super) struct Watermark {
    /// The git commit (`origin.commit`).
    pub commit: String,
    /// The import session of the lineage (`git-import:<root>`).
    pub session: String,
}

/// Why a non-empty branch cannot be imported onto. Carries the CLI message.
#[derive(Debug)]
pub(super) struct WatermarkRefusal(pub String);

/// `Ok(None)`: the branch is absent or empty (a fresh import). `Ok(Some(w))`:
/// import from `w.commit`. `Err`: refuse — native ops follow the last imported
/// one, or the branch never came from an import.
pub(super) fn derive(
    store: &Store,
    branch: &str,
) -> Result<std::result::Result<Option<Watermark>, WatermarkRefusal>> {
    let Some(head) = store.get_branch(branch)?.and_then(|b| b.head_op) else {
        return Ok(Ok(None));
    };
    let log = OpLog::open(store.root())?;
    let intents = IntentLog::open(store.root())?;
    let mut cur = head;
    let mut native = 0usize;
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert(cur.clone()) {
            break;
        }
        let Some(rec) = log.get(&cur)? else { break };
        let origin = match &rec.op.intent_id {
            Some(id) => intents.get(id)?.and_then(|i| i.origin.clone().map(|o| (o, i.session_id.clone()))),
            None => None,
        };
        if let Some((origin, session)) = origin {
            if origin.vcs != "git" {
                return Ok(Err(WatermarkRefusal(format!(
                    "store branch `{branch}` was imported from a `{}` source, not git; \
                     import into a fresh store branch with --store-branch <name>",
                    origin.vcs
                ))));
            }
            if native > 0 {
                return Ok(Err(WatermarkRefusal(format!(
                    "store branch `{branch}` has {native} native (non-imported) op(s) after the last \
                     imported commit {}; importing on top of them would interleave git history with \
                     native work. Import into a separate branch with --store-branch <name> and merge",
                    origin.commit
                ))));
            }
            return Ok(Ok(Some(Watermark { commit: origin.commit, session })));
        }
        native += 1;
        // Imported history is linear; a merge op is native by definition, and
        // following its first parent is enough to find the imported base.
        match rec.op.parents.first() {
            Some(p) => cur = p.clone(),
            None => break,
        }
    }
    Ok(Err(WatermarkRefusal(format!(
        "store branch `{branch}` already has history, but none of it came from an import (no git \
         origin on any op); this importer only extends an imported lineage — import into a fresh \
         branch with --store-branch <name>"
    ))))
}
