//! A StageId is deliberately name-independent: it hashes the
//! *structural* signature (SigId minus the name) plus the
//! implementation hash (the canonical AST with names blanked), so two
//! functions that differ only in their name share one StageId while
//! keeping two distinct SigIds — see `crates/lex-ast/src/lib.rs`'s
//! `stage_id` and `docs/INVARIANTS.md`.
//!
//! `stage_index.jsonl` maps each StageId to exactly ONE SigId, so
//! `lookup_lifecycle` — and therefore `get_ast` / `get_asts_bulk` —
//! cannot tell those two functions apart: it returns whichever of the
//! two stored ASTs the index happens to name, i.e. the right body under
//! the wrong name.
//!
//! That is how alpibrusl/lex-lang#826's republish kept emitting ops
//! forever. `pkg_publish_handler` builds its "what does the branch
//! currently hold" map by fetching every live function and keying on the
//! fetched AST's name; with a shared StageId it got two entries under
//! one name and none under the other, so the missing name matched no
//! candidate, got re-reported as an Add, and produced a fresh
//! `add_function` op on every single publish of unchanged source. The
//! branch head map is keyed by SigId and already knows both, so the fix
//! is to read through the SigId the caller holds.

use lex_ast::{canonicalize_program, sig_id, stage_id, Stage};
use lex_store::Store;
use lex_syntax::parse_source;
use tempfile::TempDir;

/// `fn <name>() -> Str { "missing" }` — identical in every respect but
/// the name, which is exactly the shape that collides. The name is set
/// on the AST rather than written into the source, because a mangled
/// name (`error_33bc9441.code_missing`) contains a `.` and is not
/// parseable — the loader produces those post-parse.
fn stage_named(name: &str) -> Stage {
    let prog = parse_source("fn placeholder() -> Str { \"missing\" }\n").expect("parse");
    let stage = canonicalize_program(&prog).into_iter().next().expect("one stage");
    match stage {
        Stage::FnDecl(mut fd) => {
            fd.name = name.to_string();
            Stage::FnDecl(fd)
        }
        other => panic!("expected a FnDecl, got {other:?}"),
    }
}

#[test]
fn two_names_one_stage_id_resolve_to_their_own_asts_by_sig() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let bare = stage_named("code_missing");
    let mangled = stage_named("error_33bc9441.code_missing");

    let bare_sig = sig_id(&bare).unwrap();
    let mangled_sig = sig_id(&mangled).unwrap();
    assert_ne!(bare_sig, mangled_sig, "the name is part of SigId");
    assert_eq!(
        stage_id(&bare).unwrap(), stage_id(&mangled).unwrap(),
        "the name is NOT part of StageId — this test is pointless if they differ",
    );

    let shared_stage = store.publish(&bare).unwrap();
    assert_eq!(store.publish(&mangled).unwrap(), shared_stage);

    // Reading through the SigId — what a branch head map gives you —
    // returns each function's own AST, under its own name.
    let pairs = vec![
        (bare_sig.clone(), shared_stage.clone()),
        (mangled_sig.clone(), shared_stage.clone()),
    ];
    let names: Vec<String> = store
        .get_asts_for_sigs_bulk(&pairs)
        .into_iter()
        .map(|r| match r.expect("both ASTs are stored, one per sig dir") {
            Stage::FnDecl(fd) => fd.name,
            other => panic!("expected a FnDecl, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["code_missing", "error_33bc9441.code_missing"]);

    // Results stay positionally aligned with the input, including for
    // an entry that cannot be resolved.
    let with_bogus = vec![
        (bare_sig.clone(), shared_stage.clone()),
        (bare_sig.clone(), "0".repeat(64)),
        (mangled_sig.clone(), shared_stage.clone()),
    ];
    let results = store.get_asts_for_sigs_bulk(&with_bogus);
    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok());
    assert!(results[1].is_err(), "an unknown stage under a known sig must be an Err");
    assert!(results[2].is_ok());

    // And the stage_id-only path is the ambiguous one it replaces: it
    // resolves to a single sig, so one of these two names is
    // unreachable through it. (Pinned as a known limitation, not as
    // desirable behavior — anything holding a SigId should not use it.)
    let by_stage_id = match store.get_ast(&shared_stage).unwrap() {
        Stage::FnDecl(fd) => fd.name,
        other => panic!("expected a FnDecl, got {other:?}"),
    };
    assert!(
        names.contains(&by_stage_id),
        "get_ast must at least return one of the two, got {by_stage_id}",
    );
}
