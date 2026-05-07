//! Integration test: build a SearchIndex over a real `lex_store::Store`
//! and confirm fusion ranking surfaces the right stage for a given
//! query. Uses [`MockEmbedder`] so the tests are deterministic and
//! offline.

use lex_ast::canonicalize_program;
use lex_search::{MockEmbedder, SearchIndex};
use lex_store::{Metadata, Store, Test};
use lex_syntax::parse_source;
use tempfile::tempdir;

/// Publish a single function and overwrite its metadata note. The
/// store doesn't currently expose a "publish-with-note" path, so we
/// rewrite the metadata.json after publish.
fn publish_with_note(store: &Store, src: &str, note: Option<&str>) -> String {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let stage = stages.first().expect("at least one stage").clone();
    let stage_id = store.publish(&stage).unwrap();
    if let Some(note) = note {
        // Find the metadata file under <root>/stages/<sig>/implementations/<stage>.metadata.json
        let sig = lex_ast::sig_id(&stage).expect("sig");
        let path = store.root()
            .join("stages")
            .join(&sig)
            .join("implementations")
            .join(format!("{stage_id}.metadata.json"));
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut meta: Metadata = serde_json::from_str(&raw).unwrap();
        meta.note = Some(note.into());
        std::fs::write(&path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
    }
    // Activate so SearchIndex picks it up.
    store.activate(&stage_id).unwrap();
    stage_id
}

#[test]
fn build_index_over_two_stages_ranks_query_correctly() {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let csv_id = publish_with_note(
        &store,
        "fn parse_csv(text :: String) -> List[String] { [] }",
        Some("parse comma-separated values into a list of rows"),
    );
    let _http_id = publish_with_note(
        &store,
        "fn send_post(url :: String, body :: String) -> String { url }",
        Some("send an HTTP POST request with a JSON body"),
    );

    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    assert_eq!(idx.stages.len(), 2, "expected both active stages indexed");

    // Query overlap (literal tokens "csv", "rows") favours parse_csv
    // under the mock embedder's hash-bag-of-words.
    let hits = idx.query(&embedder, "parse csv rows", 5).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].stage_id, csv_id,
        "parse_csv should rank first for a CSV-related query; got {:?}",
        hits.iter().map(|h| &h.name).collect::<Vec<_>>());
    assert!(hits[0].score.fused > hits[1].score.fused,
        "fused score must strictly order matching > non-matching");
}

#[test]
fn skip_unactivated_stages() {
    // Only Active stages should reach the index — drafts get skipped.
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let prog = parse_source("fn draft_only() -> Int { 0 }").expect("parse");
    let stages = canonicalize_program(&prog);
    let stage = stages.first().unwrap().clone();
    let _id = store.publish(&stage).unwrap();
    // Deliberately do NOT activate.

    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    assert!(idx.stages.is_empty(),
        "draft stages must not appear in the search index");
}

#[test]
fn signature_only_stage_still_indexes_and_ranks() {
    // A stage with no metadata.note and no examples should still
    // index — the redistribution rule lets signature-only matches
    // hit a perfect 0.8 score.
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let stage_id = publish_with_note(
        &store,
        "fn send_email(to :: String, body :: String) -> [io] Int { 0 }",
        None,  // no note
    );

    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    assert_eq!(idx.stages.len(), 1);
    assert!(idx.stages[0].description.is_none());
    assert!(idx.stages[0].description_emb.is_none());

    let hits = idx.query(&embedder, "send email body", 1).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].stage_id, stage_id);
    assert!(hits[0].score.fused > 0.0);
}

#[test]
fn examples_contribute_to_score_when_present() {
    // Two stages with the same signature and (empty) description but
    // different attached examples; the one whose examples textually
    // match the query should win.
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    // StageId is the *structural* hash (no name), so the bodies must
    // differ to avoid collapsing into one stage_id and tripping the
    // store's "activate the first sig that holds this stage" path.
    let prog_a = parse_source("fn alpha(x :: Int) -> Int { x + 1 }").expect("parse");
    let prog_b = parse_source("fn beta(x :: Int) -> Int { x + 2 }").expect("parse");
    let stage_a = canonicalize_program(&prog_a)[0].clone();
    let stage_b = canonicalize_program(&prog_b)[0].clone();
    let id_a = store.publish(&stage_a).unwrap();
    let id_b = store.publish(&stage_b).unwrap();
    store.activate(&id_a).unwrap();
    store.activate(&id_b).unwrap();
    let sig_a = lex_ast::sig_id(&stage_a).unwrap();
    let sig_b = lex_ast::sig_id(&stage_b).unwrap();

    // Attach a flavor-rich example to alpha; bland numbers to beta.
    store.attach_test(&sig_a, &Test {
        id: "ex1".into(),
        kind: "example".into(),
        input: serde_json::json!({"x": "double the input"}),
        expected_output: serde_json::json!("double output"),
        effects_allowed: vec![],
    }).unwrap();
    store.attach_test(&sig_b, &Test {
        id: "ex2".into(),
        kind: "example".into(),
        input: serde_json::json!({"x": 1}),
        expected_output: serde_json::json!(1),
        effects_allowed: vec![],
    }).unwrap();

    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    let hits = idx.query(&embedder, "double the input", 2).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].name, "alpha",
        "alpha's matching example should boost it above beta; got order: {:?}",
        hits.iter().map(|h| (&h.name, h.score.fused)).collect::<Vec<_>>());
}

#[test]
fn query_zero_limit_returns_empty() {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let _ = publish_with_note(&store, "fn whatever() -> Int { 0 }", Some("noted"));
    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    let hits = idx.query(&embedder, "anything", 0).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn empty_store_yields_empty_index_and_results() {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).expect("build");
    assert!(idx.stages.is_empty());
    let hits = idx.query(&embedder, "anything", 10).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn ranking_is_deterministic_across_calls() {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    publish_with_note(&store, "fn a() -> Int { 0 }", Some("alpha note"));
    publish_with_note(&store, "fn b() -> Int { 0 }", Some("beta note"));

    let embedder = MockEmbedder::new();
    let idx = SearchIndex::build(&store, &embedder).unwrap();
    let h1 = idx.query(&embedder, "alpha", 5).unwrap();
    let h2 = idx.query(&embedder, "alpha", 5).unwrap();
    assert_eq!(h1, h2, "search must be deterministic for repeated queries");
}
