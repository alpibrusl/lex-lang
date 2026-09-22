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
//! Paged responses (`limit=` given) list the delta in its **canonical
//! linearization** ([`HistoryIndex::topological`]): parents before
//! children, and otherwise the old order, so a linear history pages exactly
//! as it always did. Every page boundary is a position in that one order.
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
//! # Legacy `after=` paging is lossless (#971)
//!
//! Released clients page with `after=<last op received>`, which used to
//! mean "ops not in that op's ancestry". On a merge-heavy history that is
//! not "ops not yet sent": ops already sent from another line of history
//! came back, and, because the old order put some ops before their own
//! ancestors, ancestors of the page's last op that hadn't been sent yet
//! never were. On a 50k-op store a full legacy pull delivered 48,647 of
//! 49,991 ops, 1,264 of them twice.
//!
//! The server now remembers where each legacy paged response it served
//! ended ([`Continuation`], persisted under the store root so it survives a
//! restart). A later `after=X` (with `limit=`) for the same head,
//! where X is the last op of one of those pages, resumes right after X's
//! position in the same linearization: the rest of the delta, each op once,
//! with no client change. X means only "a page ended here"; which delta it
//! belongs to comes from the recorded continuation, not from X.
//!
//! Otherwise `after=X` keeps its old meaning, "ops not in X's ancestry",
//! which is also the only correct reading of a first page: that `after` is
//! the client's own branch head, and the client holds exactly its ancestry.
//! (A position-only rule would be wrong there. It would skip every op
//! ordered before X that X's ancestry doesn't contain, for example a side
//! branch merged after the client last pulled.) Because the answer is now
//! topologically ordered, this fallback can't lose ops even in the middle
//! of a pull whose continuation was forgotten (more concurrent legacy pulls
//! than [`MAX_CONTINUATIONS`], or an unwritable store root). Every ancestor
//! of X came before X and has already been delivered. The fallback can
//! re-send ops, which `OpLog::put` absorbs. With small pages over long
//! unmerged lines it can also cycle between the two lines, exactly as the
//! pre-#971 rule did, which is why continuations persist.
//!
//! The no-`limit` (unpaged) response is unchanged: `ops_since` reversed.
//!
//! Compatibility on the wire is additive: the body is the same array, the
//! header is new, and `cursor=` is a parameter older servers ignore — so a
//! client can always send `after=<last op>` alongside `cursor=` and get a
//! correct next page from either generation of server. A malformed cursor, or one naming
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
/// Page ends remembered for legacy `after=` resumption: one per legacy
/// pull in flight (each resumption replaces its own entry), ~250 bytes each.
const MAX_CONTINUATIONS: usize = 256;
/// Where they persist, under the store root, so a restart or deploy in the
/// middle of a legacy pull doesn't drop them. Best-effort: an unreadable or
/// unwritable file only costs the ancestry fallback.
const CONTINUATIONS_FILE: &str = "ops_since_continuations.json";
/// A legacy client asks for its next page immediately; a page end older
/// than this belongs to an abandoned pull (Ctrl-C, `op pull --limit`) and
/// is dropped, so it can't be mistaken for another client's cutoff later.
const CONTINUATION_TTL_SECS: u64 = 15 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The ops reachable from `index.head()` but not from `base`, in canonical
/// (topological) order, as positions into `index`.
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
    /// `None` until first loaded from [`CONTINUATIONS_FILE`].
    continuations: Option<VecDeque<Continuation>>,
}

/// "A paged response for `delta(head, base)` ended at `end`, and its last op
/// was `last`." Lets a legacy `after=<last>` resume by position. Holds ids,
/// not the delta: a delta is deterministic, so an evicted one is rebuilt
/// with the same order and `end` stays valid.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Continuation {
    head: OpId,
    last: OpId,
    base: Option<OpId>,
    end: usize,
    /// Unix seconds when the page was served; see [`CONTINUATION_TTL_SECS`].
    at: u64,
}

/// Run `f` over the continuations, loading them from disk on first use and
/// writing them back if `f` changed them.
fn with_continuations<R>(state: &State, f: impl FnOnce(&mut VecDeque<Continuation>) -> R) -> R {
    let path = state.root.join(CONTINUATIONS_FILE);
    let mut cache = state.ops_since.lock().unwrap();
    let conts = cache.continuations.get_or_insert_with(|| {
        std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    });
    let before = conts.clone();
    let cutoff = now_secs().saturating_sub(CONTINUATION_TTL_SECS);
    conts.retain(|c| c.at >= cutoff);
    let r = f(conts);
    if *conts != before {
        if let Ok(bytes) = serde_json::to_vec(&*conts) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
    r
}

/// The recorded resumption point for a legacy `after=last` on `head`, if
/// there is exactly one. The entry is consumed: the page served from it
/// records its own successor.
fn take_continuation(state: &State, head: &OpId, last: &OpId) -> Option<Continuation> {
    with_continuations(state, |conts| {
        let hits: Vec<usize> = conts
            .iter()
            .enumerate()
            .filter(|(_, c)| &c.head == head && &c.last == last)
            .map(|(i, _)| i)
            .collect();
        let first = conts[*hits.first()?].clone();
        // Two pulls whose pages ended on the same op but that started from
        // different cutoffs: the op alone can't tell them apart, so fall
        // back to the ancestry reading, which loses nothing for either.
        if hits.iter().any(|&i| (&conts[i].base, conts[i].end) != (&first.base, first.end)) {
            return None;
        }
        for &i in hits.iter().rev() {
            conts.remove(i);
        }
        Some(first)
    })
}

fn record_continuation(state: &State, c: Continuation) {
    with_continuations(state, |conts| {
        conts.retain(|o| !(o.head == c.head && o.last == c.last && o.base == c.base && o.end == c.end));
        conts.push_back(c);
        while conts.len() > MAX_CONTINUATIONS {
            conts.pop_front();
        }
    })
}

/// A cursor-following client also sends `after=<last op>` (for servers
/// that predate cursors). It doesn't need a continuation; drop any so it
/// can't be mistaken for another client's cutoff.
fn forget_continuation(state: &State, head: &OpId, last: &OpId) {
    with_continuations(state, |conts| conts.retain(|c| !(&c.head == head && &c.last == last)))
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
    let order = index.topological(&index.since(log, base)?);
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
            // Legacy paging: `after` is the last op of a page this server
            // served for this head? Resume right after it. Otherwise it's a
            // cutoff: the ops outside its ancestry. See the module docs.
            match after.as_ref().and_then(|a| take_continuation(state, &head, a)) {
                Some(c) => (head, c.base, c.end),
                None => (head, after.clone(), 0),
            }
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

    if let (Some(_), Some(a)) = (&cursor, &after) {
        forget_continuation(state, &head, a);
    }
    if end < total && cursor.is_none() {
        if let Some(last) = ops.last() {
            record_continuation(state, Continuation {
                head: head.clone(),
                last: last.op_id.clone(),
                base: base.clone(),
                end,
                at: now_secs(),
            });
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

    fn cont(last: &str, base: Option<&str>, end: usize, at: u64) -> Continuation {
        Continuation { head: "h".into(), last: last.into(), base: base.map(String::from), end, at }
    }

    #[test]
    fn a_continuation_is_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        record_continuation(&state, cont("x", None, 5, now_secs()));
        let got = take_continuation(&state, &"h".into(), &"x".into()).unwrap();
        assert_eq!((got.base, got.end), (None, 5));
        assert!(take_continuation(&state, &"h".into(), &"x".into()).is_none());
        assert!(take_continuation(&state, &"other".into(), &"x".into()).is_none());
    }

    #[test]
    fn continuations_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        record_continuation(&State::open(tmp.path().to_path_buf()).unwrap(), cont("x", Some("b"), 7, now_secs()));
        let fresh = State::open(tmp.path().to_path_buf()).unwrap();
        let got = take_continuation(&fresh, &"h".into(), &"x".into()).unwrap();
        assert_eq!((got.base.as_deref(), got.end), (Some("b"), 7));
    }

    #[test]
    fn an_abandoned_continuation_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        record_continuation(&state, cont("x", None, 5, now_secs() - CONTINUATION_TTL_SECS - 1));
        assert!(take_continuation(&state, &"h".into(), &"x".into()).is_none());
    }

    #[test]
    fn two_readings_of_one_page_end_fall_back_to_the_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        record_continuation(&state, cont("x", None, 5, now_secs()));
        record_continuation(&state, cont("x", Some("b"), 2, now_secs()));
        assert!(take_continuation(&state, &"h".into(), &"x".into()).is_none());
    }

    #[test]
    fn malformed_cursors_are_rejected() {
        for c in ["", "v1", "v2.ab._.0", "v1.ab._", "v1.ab._.x", "v1.ab._.1.2", "v1.../x._.0", "v1.ab.c/d.0"] {
            assert_eq!(decode_cursor(c), None, "{c}");
        }
    }
}
