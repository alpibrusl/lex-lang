//! #892 PR 2: `Intent.origin` — additive, hashed, and invisible to every
//! intent that does not carry one.
//!
//! The pinned ids below were computed OUTSIDE the Rust code (python
//! `hashlib.sha256` over the canonical JSON pre-image written out by hand),
//! so they are not merely the implementation agreeing with itself. The
//! origin-less ones were additionally recorded on an unmodified `main`
//! checkout before `origin` existed; they must never change.

use lex_vcs::{Intent, IntentLog, ModelDescriptor, Origin, Person};

fn anthropic() -> ModelDescriptor {
    ModelDescriptor { provider: "anthropic".into(), name: "claude-opus-4-7".into(), version: None }
}

fn git_import_model() -> ModelDescriptor {
    ModelDescriptor { provider: "git".into(), name: "import".into(), version: Some("1".into()) }
}

// ── stability of every pre-existing hash ────────────────────────────────

#[test]
fn origin_less_intent_ids_are_byte_identical_to_before_892() {
    // Pre-image: {"prompt":"fix the auth bug","session_id":"ses_abc","model":{"provider":"anthropic","name":"claude-opus-4-7"}}
    let i = Intent::with_timestamp("fix the auth bug", "ses_abc", anthropic(), None, 0);
    assert_eq!(i.intent_id, "5ede62683a249cd00afff49fdf56e8f659fe878a668c8b61e36f5fbc1de7c734");

    // With a parent intent and a versioned model.
    let i = Intent::with_timestamp(
        "import: add parser",
        "git-import:abc123",
        git_import_model(),
        Some("p1".into()),
        0,
    );
    assert_eq!(i.intent_id, "9237fc6214a7f5d963b7e497bb915881e16644263859c610ac8ce721df0f0f57");

    // With an issue (#949) as well.
    let i = i.with_issue("iss-1".into());
    assert_eq!(i.intent_id, "f64596aabe2006e357f9e48b18238daced74c4d11bdc94090d787e952bb79e17");
}

#[test]
fn origin_less_intent_serializes_without_an_origin_key() {
    let i = Intent::with_timestamp("fix the auth bug", "ses_abc", anthropic(), None, 7);
    let s = serde_json::to_string(&i).unwrap();
    assert!(!s.contains("origin"), "an intent without an origin must not mention one: {s}");
    assert_eq!(
        s,
        r#"{"intent_id":"5ede62683a249cd00afff49fdf56e8f659fe878a668c8b61e36f5fbc1de7c734","prompt":"fix the auth bug","session_id":"ses_abc","model":{"provider":"anthropic","name":"claude-opus-4-7"},"created_at":7}"#
    );
}

// ── origin: pinned golden, determinism, sensitivity ─────────────────────

fn person(name: &str, email: &str, when: i64, tz: &str) -> Person {
    Person { name: name.into(), email: email.into(), when, tz: tz.into() }
}

fn full_origin() -> Origin {
    Origin {
        vcs: "git".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        author: person("José Núñez", "jose@example.com", 1_700_000_000, "+0200"),
        committer: Some(person("Ann Committer", "ann@example.com", 1_700_000_100, "-0500")),
        parents: vec![
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ],
        folded: vec!["cccccccccccccccccccccccccccccccccccccccc".into()],
    }
}

fn import_intent(origin: Origin) -> Intent {
    Intent::with_timestamp(
        "add the parser\n\nBody line.",
        "git-import:1111111111111111111111111111111111111111",
        git_import_model(),
        None,
        // created_at is unhashed; vary it freely.
        42,
    )
    .with_origin(origin)
}

/// Golden ids computed with python hashlib over these hand-written
/// canonical pre-images (field order: view = prompt, session_id, model,
/// [parent_intent], [issue_id], [origin]; origin = vcs, commit, author,
/// [committer], parents, folded; person = name, email, when, tz; compact
/// separators; non-ASCII emitted as raw UTF-8):
///
/// full:
/// {"prompt":"add the parser\n\nBody line.","session_id":"git-import:1111111111111111111111111111111111111111","model":{"provider":"git","name":"import","version":"1"},"origin":{"vcs":"git","commit":"0123456789abcdef0123456789abcdef01234567","author":{"name":"José Núñez","email":"jose@example.com","when":1700000000,"tz":"+0200"},"committer":{"name":"Ann Committer","email":"ann@example.com","when":1700000100,"tz":"-0500"},"parents":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],"folded":["cccccccccccccccccccccccccccccccccccccccc"]}}
///
/// minimal (no committer, empty parents/folded):
/// ...,"origin":{"vcs":"git","commit":"0123...4567","author":{...},"parents":[],"folded":[]}}
#[test]
fn origin_bearing_intent_ids_match_independently_computed_goldens() {
    assert_eq!(
        import_intent(full_origin()).intent_id,
        "5ebd7579d2f2f1eb2450a54850353a46381a892158518e0290c742de282bb25d"
    );
    let mut minimal = full_origin();
    minimal.committer = None;
    minimal.parents = vec![];
    minimal.folded = vec![];
    assert_eq!(
        import_intent(minimal).intent_id,
        "f76afb72ccda690e133175feca92f074147467732e09acb623d9837e37863ecb"
    );
}

#[test]
fn same_origin_same_id_across_constructions() {
    let a = import_intent(full_origin());
    let b = import_intent(full_origin());
    assert_eq!(a.intent_id, b.intent_id);
    // created_at is unhashed.
    let mut c = import_intent(full_origin());
    c.created_at = 999_999;
    assert_eq!(a.intent_id, c.intent_id);
}

#[test]
fn different_origin_different_id() {
    let base = import_intent(full_origin()).intent_id;

    let mut o = full_origin();
    o.author.when += 1;
    assert_ne!(import_intent(o).intent_id, base, "author.when must be hashed");

    let mut o = full_origin();
    o.commit = "ffffffffffffffffffffffffffffffffffffffff".into();
    assert_ne!(import_intent(o).intent_id, base, "commit must be hashed");

    let mut o = full_origin();
    o.folded.push("dddddddddddddddddddddddddddddddddddddddd".into());
    assert_ne!(import_intent(o).intent_id, base, "folded must be hashed");

    let mut o = full_origin();
    o.author.tz = "+0100".into();
    assert_ne!(import_intent(o).intent_id, base, "author.tz must be hashed");

    let mut o = full_origin();
    o.committer = None;
    assert_ne!(import_intent(o).intent_id, base, "committer must be hashed");

    let mut o = full_origin();
    o.parents.reverse();
    assert_ne!(import_intent(o).intent_id, base, "parent order must be hashed");
}

#[test]
fn origin_changes_the_id_of_an_otherwise_identical_intent() {
    let plain = Intent::with_timestamp(
        "add the parser\n\nBody line.",
        "git-import:1111111111111111111111111111111111111111",
        git_import_model(),
        None,
        42,
    );
    assert_ne!(plain.intent_id, import_intent(full_origin()).intent_id);
}

#[test]
fn origin_and_issue_compose_in_either_order() {
    let a = import_intent(full_origin()).with_issue("iss-1".into());
    let b = Intent::with_timestamp(
        "add the parser\n\nBody line.",
        "git-import:1111111111111111111111111111111111111111",
        git_import_model(),
        None,
        42,
    )
    .with_issue("iss-1".into())
    .with_origin(full_origin());
    assert_eq!(a.intent_id, b.intent_id);
    assert_ne!(a.intent_id, import_intent(full_origin()).intent_id);
}

// ── serialization ───────────────────────────────────────────────────────

#[test]
fn origin_round_trips_through_json_and_the_intent_log() {
    let i = import_intent(full_origin());
    let json = serde_json::to_string(&i).unwrap();
    let back: Intent = serde_json::from_str(&json).unwrap();
    assert_eq!(i, back);
    assert_eq!(back.origin, Some(full_origin()));

    let tmp = tempfile::tempdir().unwrap();
    let log = IntentLog::open(tmp.path()).unwrap();
    log.put(&i).unwrap();
    assert_eq!(log.get(&i.intent_id).unwrap().unwrap(), i);
}

#[test]
fn absent_committer_is_omitted_and_missing_lists_default_to_empty() {
    let mut o = full_origin();
    o.committer = None;
    let s = serde_json::to_string(&o).unwrap();
    assert!(!s.contains("committer"), "{s}");
    // A minimal hand-written origin still parses (forward tolerance).
    let parsed: Origin = serde_json::from_str(
        r#"{"vcs":"git","commit":"abc","author":{"name":"n","email":"e","when":1,"tz":"+0000"}}"#,
    )
    .unwrap();
    assert!(parsed.committer.is_none() && parsed.parents.is_empty() && parsed.folded.is_empty());
}

/// An older reader (this crate before #892) has no `origin` field and no
/// `deny_unknown_fields`; serde's default is to ignore unknown keys. Pin the
/// same tolerance for a *newer* intent read by this reader: an unknown extra
/// key at the intent level or inside `origin` must not crash it.
#[test]
fn unknown_fields_are_tolerated_by_the_reader() {
    let i = import_intent(full_origin());
    let mut v = serde_json::to_value(&i).unwrap();
    v["future_field"] = serde_json::json!({"x": 1});
    v["origin"]["future_origin_field"] = serde_json::json!("y");
    v["origin"]["author"]["future_person_field"] = serde_json::json!(3);
    let back: Intent = serde_json::from_value(v).unwrap();
    assert_eq!(back, i);
}
