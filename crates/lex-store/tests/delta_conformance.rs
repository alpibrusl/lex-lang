//! Conformance tests for #261 slice 3: delta-encoded stage bytes.

use lex_ast::{canonicalize_program, Stage};
use lex_store::{Store, StageStatus};
use lex_syntax::parse_source;
use tempfile::TempDir;

fn stage_named(src: &str, name: &str) -> Stage {
    let prog = parse_source(src).unwrap();
    canonicalize_program(&prog)
        .into_iter()
        .find(|s| match s {
            Stage::FnDecl(fd) => fd.name == name,
            _ => false,
        })
        .expect("stage not found")
}

fn impl_dir(root: &std::path::Path, sig: &str) -> std::path::PathBuf {
    root.join("stages").join(sig).join("implementations")
}

#[test]
fn first_stage_published_is_a_full_snapshot() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = stage_named("fn double(n :: Int) -> Int { n * 2 }\n", "double");
    let stage_id = store.publish(&s).unwrap();
    let sig = lex_ast::sig_id(&s).unwrap();
    let dir = impl_dir(tmp.path(), &sig);
    assert!(dir.join(format!("{stage_id}.ast.json")).exists(),
        "first stage must be a full .ast.json snapshot");
    assert!(!dir.join(format!("{stage_id}.delta.json")).exists());
}

#[test]
fn second_close_stage_is_delta_encoded() {
    // Two function bodies that share most of their canonical-JSON
    // bytes — `n * 2` vs `n * 3`, single-character difference.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s1 = stage_named("fn double(n :: Int) -> Int { n * 2 }\n", "double");
    let s2 = stage_named("fn double(n :: Int) -> Int { n * 3 }\n", "double");
    let id1 = store.publish(&s1).unwrap();
    let id2 = store.publish(&s2).unwrap();
    assert_ne!(id1, id2, "different bodies ⇒ different stage_ids");

    let sig = lex_ast::sig_id(&s2).unwrap();
    let dir = impl_dir(tmp.path(), &sig);
    assert!(dir.join(format!("{id1}.ast.json")).exists(),
        "the first stage stays a full snapshot");
    assert!(dir.join(format!("{id2}.delta.json")).exists(),
        "the second stage should be delta-encoded against the first");
    assert!(!dir.join(format!("{id2}.ast.json")).exists(),
        "no full .ast.json for the delta-encoded stage");
}

#[test]
fn get_ast_reconstructs_through_delta() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s1 = stage_named("fn triple(n :: Int) -> Int { n * 3 }\n", "triple");
    let s2 = stage_named("fn triple(n :: Int) -> Int { n * 4 }\n", "triple");
    let _ = store.publish(&s1).unwrap();
    let id2 = store.publish(&s2).unwrap();

    let reconstructed = store.get_ast(&id2).unwrap();
    assert_eq!(reconstructed, s2,
        "get_ast must transparently reconstruct delta-encoded stages");
}

#[test]
fn lifecycle_commands_work_against_delta_encoded_stages() {
    // Activating, deprecating, and tombstoning rely on get_status,
    // which reads lifecycle.json — independent of how the AST
    // bytes are stored. But callers also routinely fetch `get_ast`
    // for activated stages; sanity-check the round trip.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s1 = stage_named("fn quad(n :: Int) -> Int { n * 4 }\n", "quad");
    let s2 = stage_named("fn quad(n :: Int) -> Int { n * 5 }\n", "quad");
    let _ = store.publish(&s1).unwrap();
    let id2 = store.publish(&s2).unwrap();

    store.activate(&id2).unwrap();
    assert_eq!(store.get_status(&id2).unwrap(), StageStatus::Active);
    let back = store.get_ast(&id2).unwrap();
    assert_eq!(back, s2);
}

#[test]
fn republishing_an_existing_delta_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s1 = stage_named("fn idem(n :: Int) -> Int { n + 1 }\n", "idem");
    let s2 = stage_named("fn idem(n :: Int) -> Int { n + 2 }\n", "idem");
    let _ = store.publish(&s1).unwrap();
    let id2_first = store.publish(&s2).unwrap();
    let id2_second = store.publish(&s2).unwrap();
    assert_eq!(id2_first, id2_second);

    let sig = lex_ast::sig_id(&s2).unwrap();
    let dir = impl_dir(tmp.path(), &sig);
    // Still exactly one delta file for s2 — no full .ast.json
    // crept in on the second publish.
    assert!(dir.join(format!("{id2_first}.delta.json")).exists());
    assert!(!dir.join(format!("{id2_first}.ast.json")).exists());
}

#[test]
fn deep_chain_eventually_materializes_a_snapshot() {
    // Publish DELTA_CHAIN_CAP+1 stages all linearly close in
    // bytes. The last one must materialize as a full snapshot so
    // reconstruction stays bounded.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let mut last_id = String::new();
    let mut last_stage: Option<Stage> = None;
    // Each iteration's body literal has a unique multi-digit
    // constant — different bytes each round.
    for i in 0..(lex_store::DELTA_CHAIN_CAP + 2) {
        let src = format!("fn chain(n :: Int) -> Int {{ n + {} }}\n", i);
        let s = stage_named(&src, "chain");
        last_id = store.publish(&s).unwrap();
        last_stage = Some(s);
    }

    let sig = lex_ast::sig_id(last_stage.as_ref().unwrap()).unwrap();
    let dir = impl_dir(tmp.path(), &sig);
    let last_path_ast = dir.join(format!("{last_id}.ast.json"));
    let last_path_delta = dir.join(format!("{last_id}.delta.json"));
    // The LAST publish should be a snapshot once we've gone past
    // the cap. Either it's stored as `.ast.json` or as a delta
    // with chain_length <= cap (capped + reset). Allow both.
    assert!(last_path_ast.exists() || last_path_delta.exists());
    if last_path_delta.exists() {
        let bytes = std::fs::read(&last_path_delta).unwrap();
        let delta: lex_store::StageDelta = serde_json::from_slice(&bytes).unwrap();
        assert!(delta.chain_length <= lex_store::DELTA_CHAIN_CAP,
            "chain_length {} must not exceed cap {}",
            delta.chain_length, lex_store::DELTA_CHAIN_CAP);
    }
    // get_ast still works on the last stage regardless of how it
    // was stored.
    let back = store.get_ast(&last_id).unwrap();
    assert_eq!(back, last_stage.unwrap());
}

#[test]
fn dissimilar_second_stage_is_a_full_snapshot() {
    // When the byte diff against the prior stage is over the
    // threshold, the publish path falls back to a full snapshot.
    // Use bodies that share little: tiny vs large.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s1 = stage_named("fn diff(n :: Int) -> Int { 1 }\n", "diff");
    let s2 = stage_named(
        "fn diff(n :: Int) -> Int { let x := n * n; let y := x + 1; let z := y - 1; x + y + z }\n",
        "diff",
    );
    let _ = store.publish(&s1).unwrap();
    let id2 = store.publish(&s2).unwrap();

    let sig = lex_ast::sig_id(&s2).unwrap();
    let dir = impl_dir(tmp.path(), &sig);
    assert!(dir.join(format!("{id2}.ast.json")).exists(),
        "dissimilar bodies should fall back to a full snapshot");
}
