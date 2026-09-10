//! `Store::lookup_lifecycle` (used by `get_ast`, `activate`,
//! `deprecate`, `tombstone`) used to find which SigId owns a
//! StageId by scanning *every* SigId in the tenant (`list_sigs()`,
//! not scoped to any one package) and reading+parsing each one's
//! `lifecycle.json` until a match turned up.
//!
//! `pkg_publish_handler` (`lex-api/src/handlers.rs`) calls `get_ast`
//! once per pre-existing function in the tenant to build
//! `old_fns_by_name` — so on a tenant with a few thousand published
//! functions, a single publish request turned into millions of
//! individual file reads. Measured at roughly an hour on the
//! `alpibrusl` tenant's ~2,400-function store — independently of,
//! and in addition to, the separate `branch_head` O(N)-per-call
//! issue (alpibrusl/lex-lang#821).
//!
//! The fix adds a persisted reverse index (`stage_index.jsonl`,
//! StageId -> SigId) that `publish_signed` populates eagerly for
//! every newly-created stage_id, and that `lookup_lifecycle` checks
//! first — a single sequential read of one (typically small) file
//! instead of opening and parsing up to N separate lifecycle files.
//! A miss (index doesn't exist yet, or predates the fix) falls back
//! to the old full scan and backfills the index.

use lex_ast::{canonicalize_program, Stage};
use lex_store::Store;
use lex_syntax::parse_source;
use std::time::Instant;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

/// A trivially small, distinctly-named/bodied fn — canonicalizes to
/// a distinct SigId (name is part of it) and StageId (body differs)
/// for every `i`.
fn make_stage(i: usize) -> Stage {
    let src = format!("fn f{i}(n :: Int) -> Int {{ n + {i} }}\n");
    let prog = parse_source(&src).unwrap();
    canonicalize_program(&prog).into_iter().next().unwrap()
}

#[test]
fn get_ast_is_correct_even_when_the_index_is_missing_or_stale() {
    let (store, tmp) = fresh();

    let mut ids = Vec::new();
    for i in 0..50 {
        let stage = make_stage(i);
        let stage_id = store.publish(&stage).unwrap();
        ids.push((stage_id, stage));
    }

    // Simulate data published before this fix existed: no index file
    // at all. Every lookup must fall back to the full scan and still
    // return the right AST.
    let index_path = tmp.path().join("stage_index.jsonl");
    assert!(index_path.exists(), "publish should have created the index eagerly");
    std::fs::remove_file(&index_path).unwrap();

    for (stage_id, expected) in &ids {
        let got = store.get_ast(stage_id).unwrap();
        assert_eq!(&got, expected, "wrong AST for {stage_id} with no index present");
    }

    // The full-scan fallback must have backfilled the index — a
    // second pass should still be correct (now via the index-hit
    // path) and shouldn't need the scan again.
    assert!(index_path.exists(), "fallback should self-heal the index");
    for (stage_id, expected) in &ids {
        let got = store.get_ast(stage_id).unwrap();
        assert_eq!(&got, expected, "wrong AST for {stage_id} on second pass (index hit)");
    }
}

#[test]
fn get_ast_is_correct_with_a_stale_wrong_index_entry() {
    // Belt-and-suspenders: if the index ever pointed at the wrong
    // sig (shouldn't happen — sig ownership of a stage_id is
    // permanent — but a corrupted file is possible), lookup must not
    // trust it blindly; it should fall through to the real scan.
    let (store, tmp) = fresh();
    let stage = make_stage(0);
    let stage_id = store.publish(&stage).unwrap();

    let index_path = tmp.path().join("stage_index.jsonl");
    std::fs::write(&index_path, format!("{{\"stage_id\":\"{stage_id}\",\"sig_id\":\"totally::wrong::sig\"}}\n")).unwrap();

    let got = store.get_ast(&stage_id).unwrap();
    assert_eq!(got, stage, "a wrong index entry must not be trusted over the real scan");
}

// History size for the steady-state budget test. Chosen the same
// way `branch_perf.rs` and `branch_head_perf.rs` chose theirs: large
// enough to make an O(N) blowup obvious, small enough that building
// the fixture itself doesn't dominate CI time.
const FN_COUNT: usize = 1_500;

#[test]
fn old_fns_by_name_style_workload_stays_fast() {
    let (store, _tmp) = fresh();

    let mut ids = Vec::with_capacity(FN_COUNT);
    for i in 0..FN_COUNT {
        let stage = make_stage(i);
        ids.push(store.publish(&stage).unwrap());
    }

    // The exact shape of `pkg_publish_handler` building
    // `old_fns_by_name`: one `get_ast` call per pre-existing
    // function in the tenant.
    let start = Instant::now();
    for stage_id in &ids {
        let _ = store.get_ast(stage_id).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 8.0,
        "{FN_COUNT} get_ast calls against a {FN_COUNT}-function tenant took {elapsed:?}; \
         expected the reverse index to keep this well under a full tenant-wide scan per call"
    );
}

/// The lazy per-lookup fallback (the fix above) is fine for an
/// occasional individual miss, but pathological as a *bulk*
/// cold-start strategy: reopening a store that has thousands of
/// pre-existing functions but no index yet (legacy data, or an
/// index file lost some other way) would otherwise mean every one
/// of those functions rediscovers itself the slow way -- O(total
/// sigs) per miss, O(total sigs^2) overall for an `old_fns_by_name`-
/// shaped workload right after reopening. `Store::open` now runs a
/// single O(total sigs) bulk pass (`rebuild_stage_index`) up front
/// whenever the index file is missing, so this scenario stays fast
/// too.
#[test]
fn reopening_a_store_with_legacy_data_bulk_rebuilds_instead_of_relying_on_lazy_fallback() {
    let tmp = TempDir::new().unwrap();
    let mut ids = Vec::with_capacity(FN_COUNT);
    {
        let store = Store::open(tmp.path()).unwrap();
        for i in 0..FN_COUNT {
            ids.push(store.publish(&make_stage(i)).unwrap());
        }
    }
    // Simulate data published before the index existed at all.
    let index_path = tmp.path().join("stage_index.jsonl");
    std::fs::remove_file(&index_path).unwrap();

    let start = Instant::now();
    let store = Store::open(tmp.path()).unwrap();
    assert!(index_path.exists(), "Store::open should rebuild a missing index up front");
    for stage_id in &ids {
        let _ = store.get_ast(stage_id).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 8.0,
        "open + {FN_COUNT} get_ast calls against reopened legacy data took {elapsed:?}; \
         expected a one-pass bulk rebuild on open, not the O(N^2) lazy per-call fallback"
    );
}
