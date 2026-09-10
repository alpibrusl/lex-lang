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
    // Simulate data published before the index existed at all: no
    // index file AND no completion marker (the marker from the
    // `fresh()` open above, made while the store was still empty,
    // would otherwise wrongly tell the next open "nothing to do").
    let index_path = tmp.path().join("stage_index.jsonl");
    let marker_path = tmp.path().join("stage_index.complete");
    std::fs::remove_file(&index_path).unwrap();
    std::fs::remove_file(&marker_path).unwrap();

    let start = Instant::now();
    let store = Store::open(tmp.path()).unwrap();
    assert!(index_path.exists(), "Store::open should rebuild a missing index up front");
    assert!(marker_path.exists(), "Store::open should mark the rebuild complete");
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

/// The scenario this caught in production: an earlier bulk-rebuild
/// pass was interrupted (a server restart, a killed process) partway
/// through, leaving `stage_index.jsonl` on disk with *some* entries
/// but not all, and critically no completion marker. Gating solely
/// on the index file's existence (rather than a marker written only
/// after a full pass completes) would treat that partial file as
/// "already done" and fall back to the slow per-call path for
/// whatever's left -- silently reproducing the exact O(N^2) cost this
/// fix exists to avoid, just for a smaller remaining N.
#[test]
fn reopening_a_store_with_a_partial_index_and_no_marker_finishes_the_rebuild() {
    let tmp = TempDir::new().unwrap();
    let mut ids = Vec::with_capacity(FN_COUNT);
    {
        let store = Store::open(tmp.path()).unwrap();
        for i in 0..FN_COUNT {
            ids.push(store.publish(&make_stage(i)).unwrap());
        }
    }
    // Truncate the index down to a handful of entries and remove the
    // marker -- exactly what an interrupted rebuild leaves behind.
    let index_path = tmp.path().join("stage_index.jsonl");
    let marker_path = tmp.path().join("stage_index.complete");
    let content = std::fs::read_to_string(&index_path).unwrap();
    let partial: String = content.lines().take(5).map(|l| format!("{l}\n")).collect();
    std::fs::write(&index_path, partial).unwrap();
    std::fs::remove_file(&marker_path).unwrap();

    let start = Instant::now();
    let store = Store::open(tmp.path()).unwrap();
    assert!(marker_path.exists(), "Store::open should finish the interrupted rebuild and mark it complete");
    for stage_id in &ids {
        let _ = store.get_ast(stage_id).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 8.0,
        "open + {FN_COUNT} get_ast calls against a partially-rebuilt index took {elapsed:?}; \
         expected the interrupted rebuild to be resumed and finished on open, not left partial"
    );
}

/// The bug this caught in production (#825): `stage_index.jsonl` can
/// only record what a full scan *finds* -- a stage_id that's
/// genuinely orphaned (referenced by the branch's current head, but
/// missing from every sig's lifecycle -- real data on the `alpibrusl`
/// tenant, ~27% of its live functions) can never be indexed by the
/// bulk rebuild. Every `get_ast` call for one of those redid the full
/// O(total sigs) scan, found nothing, and gave up -- forever, on
/// every single call, since nothing remembered the failure. Measured
/// directly: 3,664 old_fns_by_name lookups against the real tenant
/// data, 988 of them for orphaned stage_ids, took 358s locally (and
/// 40+ minutes in production) almost entirely from re-scanning for
/// the same permanently-missing entries over and over.
#[test]
fn repeated_lookups_of_a_permanently_missing_stage_id_stay_fast() {
    let (store, _tmp) = fresh();
    for i in 0..FN_COUNT {
        store.publish(&make_stage(i)).unwrap();
    }

    let ghost = "sha256-of-something-that-was-never-published";
    // First call: genuinely not found anywhere, must still fail --
    // and must cache that fact.
    assert!(store.get_ast(ghost).is_err());

    // FN_COUNT repeats of the SAME missing lookup: without caching
    // the negative result, each one redoes a full O(FN_COUNT) scan --
    // O(FN_COUNT^2) overall, the same shape as the bug this test
    // guards against.
    let start = Instant::now();
    for _ in 0..FN_COUNT {
        assert!(store.get_ast(ghost).is_err());
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 8.0,
        "{FN_COUNT} repeated lookups of one permanently-missing stage_id took {elapsed:?}; \
         expected the negative result to be cached after the first full scan, not rescanned every call"
    );
}

#[test]
fn get_asts_bulk_matches_individual_get_ast_calls() {
    let (store, _tmp) = fresh();
    let mut ids = Vec::new();
    for i in 0..30 {
        ids.push(store.publish(&make_stage(i)).unwrap());
    }
    // Interleave a few permanently-missing ids, matching the real
    // production shape (a mix of resolvable and orphaned entries).
    let mut lookup_ids = ids.clone();
    for i in 0..5 {
        lookup_ids.insert(i * 5, format!("ghost-{i}"));
    }

    let individual: Vec<Option<Stage>> = lookup_ids.iter()
        .map(|id| store.get_ast(id).ok())
        .collect();
    let bulk: Vec<Option<Stage>> = store.get_asts_bulk(&lookup_ids)
        .into_iter()
        .map(|r| r.ok())
        .collect();

    assert_eq!(individual, bulk, "get_asts_bulk must return the exact same results, in the same order, as calling get_ast individually");
    assert_eq!(bulk.iter().filter(|s| s.is_some()).count(), 30, "sanity: all 30 real stage_ids should resolve");
}

/// The remaining cost after the negative-cache fix (#825): every
/// `get_ast` call re-reads and re-parses the *entire* index file, so
/// a loop of N calls costs O(index size x N) even when every call is
/// individually an index hit. Measured directly against real
/// production data: 87.6s for 3,664 calls against a ~14k-line index.
/// `get_asts_bulk` loads the index once for the whole batch instead.
#[test]
fn get_asts_bulk_is_faster_than_a_get_ast_loop_at_scale() {
    const N: usize = 3_000;
    let (store, _tmp) = fresh();
    let mut ids = Vec::with_capacity(N);
    for i in 0..N {
        ids.push(store.publish(&make_stage(i)).unwrap());
    }
    // Mirror production's ~27% orphaned-lookup ratio so both paths
    // pay the same one-time negative-cache population cost first.
    let mut lookup_ids = ids.clone();
    for i in 0..(N / 4) {
        lookup_ids.push(format!("ghost-{i}"));
    }
    // Prime the negative cache for the ghosts once, outside the
    // timed sections, so both timings below measure steady-state
    // (index-hit-only) cost, not the one-time full-scan cost that's
    // already covered by the tests above.
    for i in 0..(N / 4) {
        let _ = store.get_ast(&format!("ghost-{i}"));
    }

    let start_individual = Instant::now();
    for id in &lookup_ids {
        let _ = store.get_ast(id);
    }
    let individual_elapsed = start_individual.elapsed();

    let start_bulk = Instant::now();
    let _ = store.get_asts_bulk(&lookup_ids);
    let bulk_elapsed = start_bulk.elapsed();

    assert!(
        bulk_elapsed.as_secs_f64() * 3.0 < individual_elapsed.as_secs_f64(),
        "get_asts_bulk ({bulk_elapsed:?}) should be at least 3x faster than \
         an equivalent get_ast loop ({individual_elapsed:?}) at N={N} lookups \
         against a similarly-sized index"
    );
}
