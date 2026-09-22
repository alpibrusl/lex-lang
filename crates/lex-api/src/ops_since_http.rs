//! `GET /v1/ops/since` — the op half of `lex op pull` (#260), paged
//! without re-walking history on every page (#971).
//!
//! # Contract
//!
//! `GET /v1/ops/since?after=<op_id>&branch=<name>&limit=<n>` returns a JSON
//! array of `OperationRecord`s reachable from `branch.head_op` but not from
//! `after`, sorted **oldest-first** so the client can apply them in order
//! with `OpLog::put`. `branch` defaults to `main`; `limit` caps the page.
//! Empty array when the branch doesn't exist, has no head, or `after` is at
//! or past it. `400` for a malformed query.
//!
//! # Paging (#971)
//!
//! A full pull pages through the delta. The legacy way — still honored,
//! unchanged, for every released client — is to re-ask with
//! `after=<last op of the previous page>`. Answering that from the log on
//! disk costs a walk of the whole history per page, so a full pull of an
//! N-op store was O(N²/page): 136k ops in pages of 1000 re-read the log
//! 137 times.
//!
//! Two things now bound a page's cost:
//!
//! 1. **One walk per head, not per page.** The first request against a
//!    head indexes its history in memory ([`lex_vcs::HistoryIndex`]: ids
//!    and parent edges only, no records). Later pages — legacy `after=` ones included
//!    — compute their delta from that index in memory and read from disk
//!    only the records they return.
//! 2. **A resumable cursor.** Every page that leaves ops behind carries an
//!    `X-Lex-Next-Cursor` response header. Passing it back as `cursor=`
//!    resumes at an offset into the cached delta: O(page), no delta
//!    recomputation. The cursor pins the head and cutoff the pull started
//!    from, so a branch that moves mid-pull can't splice two histories
//!    into one pull; concatenating the pages yields exactly the unpaged
//!    response. It is opaque to clients and survives a cache eviction or a
//!    server restart (the index is rebuilt; the order is deterministic).
//!
//! Compatibility is additive: the body is the same array, the header is
//! new, and `cursor=` is a parameter older servers ignore — so a client can
//! always send `after=<last op>` alongside `cursor=` and get a correct next
//! page from either generation of server. A malformed cursor, or one naming
//! a head this store doesn't have, is a `400`, never an empty page a client
//! would mistake for "done".

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::Arc;

use lex_vcs::{HistoryIndex, OpId, OpLog, OperationRecord};
use tiny_http::{Header, Response};

use crate::handlers::{error_response, json_response, State};

/// Response header carrying the cursor for the next page.
pub const NEXT_CURSOR_HEADER: &str = "X-Lex-Next-Cursor";

/// How many head indexes and paged deltas a tenant keeps. A pull uses one
/// of each; a legacy `after=` pull churns deltas (one per page) but reuses
/// the index. An index costs ~200 bytes per op (each id is held twice,
/// in order and in the position map): ~25 MB at 136k ops.
const MAX_INDEXES: usize = 2;
const MAX_DELTAS: usize = 8;

/// The ops reachable from `index.head()` but not from `base`, oldest-first,
/// as positions into `index`.
struct Delta {
    index: Arc<HistoryIndex>,
    base: Option<OpId>,
    order: Vec<u32>,
}

/// Per-tenant cache behind `/v1/ops/since`; lives in [`State`]. Small MRU
/// lists, most recent last. Entries never go stale: op ids are content
/// addressed, so the history below a head is immutable.
#[derive(Default)]
pub(crate) struct OpsSinceCache {
    indexes: VecDeque<Arc<HistoryIndex>>,
    deltas: VecDeque<Arc<Delta>>,
}

fn touch<T>(list: &mut VecDeque<Arc<T>>, i: usize) -> Arc<T> {
    let e = list.remove(i).expect("index in range");
    list.push_back(Arc::clone(&e));
    e
}

fn insert<T>(list: &mut VecDeque<Arc<T>>, e: Arc<T>, cap: usize) {
    list.push_back(e);
    while list.len() > cap {
        list.pop_front();
    }
}

fn delta_for(
    state: &State,
    log: &OpLog,
    head: &OpId,
    base: Option<&OpId>,
) -> std::io::Result<Arc<Delta>> {
    let index = {
        let mut cache = state.ops_since.lock().unwrap();
        if let Some(i) = cache.deltas.iter().position(|d| d.index.head() == head && d.base.as_ref() == base) {
            return Ok(touch(&mut cache.deltas, i));
        }
        cache.indexes.iter().position(|x| x.head() == head).map(|i| touch(&mut cache.indexes, i))
    };
    // Build outside the cache lock (callers hold the store lock, which
    // already serializes a tenant's requests).
    let index = match index {
        Some(i) => i,
        None => {
            let i = Arc::new(HistoryIndex::build(log, head)?);
            insert(&mut state.ops_since.lock().unwrap().indexes, Arc::clone(&i), MAX_INDEXES);
            i
        }
    };
    let order = index.since(log, base)?;
    let delta = Arc::new(Delta { index, base: base.cloned(), order });
    insert(&mut state.ops_since.lock().unwrap().deltas, Arc::clone(&delta), MAX_DELTAS);
    Ok(delta)
}

fn is_op_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `v1.<head>.<base or _>.<offset>`.
fn encode_cursor(head: &str, base: Option<&str>, offset: usize) -> String {
    format!("v1.{head}.{}.{offset}", base.unwrap_or("_"))
}

fn decode_cursor(c: &str) -> Option<(OpId, Option<OpId>, usize)> {
    let mut parts = c.split('.');
    let (v, head, base, offset) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if v != "v1" || parts.next().is_some() || !is_op_id(head) {
        return None;
    }
    let base = match base {
        "_" => None,
        b if is_op_id(b) => Some(b.to_string()),
        _ => return None,
    };
    Some((head.to_string(), base, offset.parse().ok()?))
}

pub(crate) fn ops_since_handler(state: &State, query: &str) -> Response<Cursor<Vec<u8>>> {
    let mut after: Option<String> = None;
    let mut branch = String::from("main");
    let mut limit: Option<usize> = None;
    let mut cursor: Option<String> = None;
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "after" => after = Some(v.to_string()),
            "branch" => branch = v.to_string(),
            "cursor" => cursor = Some(v.to_string()),
            "limit" => {
                limit = Some(match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return error_response(400,
                        format!("limit must be a positive integer, got `{v}`")),
                });
            }
            _ => {}
        }
    }

    let store = state.store.lock().unwrap();
    let log = match OpLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening op log: {e}")),
    };

    // A cursor names its own head and cutoff; `branch`/`after` are only
    // there for servers that predate cursors.
    let (head, base, offset) = match &cursor {
        Some(c) => match decode_cursor(c) {
            Some(parsed) => parsed,
            None => return error_response(400, format!("malformed cursor `{c}`")),
        },
        None => {
            let head = match store.get_branch(&branch) {
                Ok(Some(b)) => b.head_op,
                Ok(None) => None,
                Err(e) => return error_response(500, format!("get_branch: {e}")),
            };
            let Some(head) = head else {
                return json_response(200, &serde_json::json!([]));
            };
            if limit.is_none() {
                // Unpaged: one answer, nothing to resume — the plain walk
                // is already the cheapest way to produce it.
                return match log.ops_since(&head, after.as_ref()) {
                    Ok(mut ops) => {
                        ops.reverse();
                        json_response(200, &serde_json::to_value(&ops).unwrap_or_default())
                    }
                    Err(e) => error_response(500, format!("ops_since: {e}")),
                };
            }
            (head, after, 0)
        }
    };

    let delta = match delta_for(state, &log, &head, base.as_ref()) {
        Ok(d) => d,
        Err(e) => return error_response(500, format!("ops_since: {e}")),
    };
    let total = delta.order.len();
    if cursor.is_some() && (delta.index.is_empty() || offset > total) {
        return error_response(400, format!(
            "cursor `{}` does not name a page of this store's history; restart the pull",
            cursor.unwrap_or_default()));
    }
    let end = match limit {
        Some(n) => offset.saturating_add(n).min(total),
        None => total,
    };
    let mut ops: Vec<OperationRecord> = Vec::with_capacity(end - offset);
    for &i in &delta.order[offset..end] {
        match log.get(delta.index.id(i)) {
            Ok(Some(rec)) => ops.push(rec),
            // Evicted by `lex op gc` since the index was built.
            Ok(None) => {}
            Err(e) => return error_response(500, format!("reading op {}: {e}", delta.index.id(i))),
        }
    }

    let resp = json_response(200, &serde_json::to_value(&ops).unwrap_or_default());
    if end < total {
        let next = encode_cursor(&head, base.as_deref(), end);
        if let Ok(h) = Header::from_bytes(NEXT_CURSOR_HEADER.as_bytes(), next.as_bytes()) {
            return resp.with_header(h);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let h = "ab".repeat(32);
        for base in [None, Some("cd".repeat(32))] {
            let c = encode_cursor(&h, base.as_deref(), 1000);
            assert_eq!(decode_cursor(&c), Some((h.clone(), base, 1000)));
        }
    }

    #[test]
    fn malformed_cursors_are_rejected() {
        for c in ["", "v1", "v2.ab._.0", "v1.ab._", "v1.ab._.x", "v1.ab._.1.2", "v1.../x._.0", "v1.ab.c/d.0"] {
            assert_eq!(decode_cursor(c), None, "{c}");
        }
    }
}
