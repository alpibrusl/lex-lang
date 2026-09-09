//! A level is only accepted on a dimension that gives it a meaning
//! (alpibrusl/lex-lang#808), and a grant refuses keys nobody defined.
//!
//! The vocabulary is shared across dimensions on purpose — it is small
//! and the ordering is the point — but sharing a vocabulary is not
//! sharing a meaning. `exec: Allowlist` was the case that showed why
//! the difference matters: it parsed, it outranked `Sandboxed`, it
//! narrowed cleanly under `exec: Full`, and it resolved to a *weaker*
//! isolation floor than the full-exec reading it looked like.
//!
//! Found while chasing a manifest author who could not work out what
//! exec levels exist (alpibrusl/lex-os#89).

use lex_types::trust::{Dimension, Grant, Level, TrustError};

fn parse(s: &str) -> Result<Grant, serde_json::Error> {
    serde_json::from_str::<Grant>(s)
}

#[test]
fn a_well_formed_grant_still_parses() {
    // The negative control: if this fails, every refusal below is
    // refusing for the wrong reason.
    let g = parse(r#"{"filesystem":"ReadOnly","network":"Allowlist","exec":"Sandboxed"}"#)
        .expect("the canonical shape must parse");
    assert_eq!(g.filesystem, Level::ReadOnly);
    assert_eq!(g.network, Level::Allowlist);
    assert_eq!(g.exec, Level::Sandboxed);
}

/// The specific grant that motivated the fix.
#[test]
fn exec_allowlist_is_refused() {
    let err = parse(r#"{"filesystem":"None","network":"None","exec":"Allowlist"}"#)
        .expect_err("`Allowlist` is a network word; exec must refuse it");

    // The message has to say what exec *does* accept, or an author who
    // guessed once simply guesses again.
    let msg = err.to_string();
    assert!(msg.contains("exec"), "{msg}");
    // The JSON spelling, not the prose one: this message is read by
    // whoever is writing the manifest.
    assert!(
        msg.contains("`Sandboxed`"),
        "the refusal must quote the spelling the parser accepts: {msg}"
    );
    assert!(
        !msg.contains("`sandboxed`"),
        "quoting the lowercase prose form would send an author round again: {msg}"
    );
}

#[test]
fn a_filesystem_word_is_refused_on_the_network() {
    assert!(parse(r#"{"filesystem":"Full","network":"ReadWrite","exec":"None"}"#).is_err());
}

#[test]
fn a_network_word_is_refused_on_the_filesystem() {
    assert!(parse(r#"{"filesystem":"Loopback","network":"Full","exec":"None"}"#).is_err());
}

/// `exec: ReadOnly` ranks 1, exactly like `Sandboxed`, so it was
/// harmless in every check — and that is precisely why it should not be
/// writable. A grant with two spellings has two names, and a name is
/// what every audit record refers to.
#[test]
fn a_harmless_looking_exec_alias_is_still_refused() {
    assert!(parse(r#"{"filesystem":"None","network":"None","exec":"ReadOnly"}"#).is_err());
}

/// Every dimension accepts every one of its own levels — the other half
/// of the test above, without which "refuse everything" would pass.
#[test]
fn each_dimension_accepts_all_of_its_own_levels() {
    for d in Dimension::ALL {
        for &l in d.levels() {
            assert!(
                d.permits_level(l),
                "{d} must accept `{l}`, which it lists as its own"
            );
            let g = match d {
                Dimension::Filesystem => Grant::try_new(l, Level::None, Level::None),
                Dimension::Network => Grant::try_new(Level::None, l, Level::None),
                Dimension::Exec => Grant::try_new(Level::None, Level::None, l),
            };
            assert!(g.is_ok(), "{d} = `{l}` must construct");
        }
    }
}

/// The other half of #101's finding, one level down: an unknown key
/// inside `grant` was silently dropped, so an author could believe they
/// had declared a fourth dimension.
#[test]
fn an_invented_dimension_is_refused_rather_than_dropped() {
    let err = parse(r#"{"filesystem":"None","network":"None","exec":"None","gpu":"Full"}"#)
        .expect_err("an unknown dimension must be refused");
    assert!(
        err.to_string().contains("gpu"),
        "the refusal must name it: {err}"
    );
}

#[test]
fn a_missing_dimension_is_still_refused() {
    assert!(
        parse(r#"{"filesystem":"None","network":"None"}"#).is_err(),
        "all three dimensions are required"
    );
}

/// `try_new` and the deserializer must agree, or a grant would be
/// writable through one door and not the other.
#[test]
fn try_new_and_deserialization_agree() {
    let cases = [
        (Level::None, Level::None, Level::Allowlist, false),
        (Level::Loopback, Level::None, Level::None, false),
        (Level::None, Level::ReadWrite, Level::None, false),
        (Level::ReadWrite, Level::Allowlist, Level::Sandboxed, true),
        (Level::Full, Level::Full, Level::Full, true),
        (Level::None, Level::None, Level::None, true),
    ];
    for (fs, net, exec, expected_ok) in cases {
        let via_ctor = Grant::try_new(fs, net, exec).is_ok();
        let json = format!(
            r#"{{"filesystem":"{}","network":"{}","exec":"{}"}}"#,
            json_name(fs),
            json_name(net),
            json_name(exec)
        );
        let via_serde = parse(&json).is_ok();
        assert_eq!(via_ctor, via_serde, "try_new and serde disagree on {json}");
        assert_eq!(via_ctor, expected_ok, "unexpected verdict for {json}");
    }
}

fn json_name(l: Level) -> String {
    serde_json::to_value(l)
        .unwrap()
        .as_str()
        .expect("Level serialises as a string")
        .to_string()
}

/// A refusal must name the dimension, so an author knows which of the
/// three to fix rather than trying each in turn.
#[test]
fn the_error_names_the_dimension_and_the_level() {
    match Grant::try_new(Level::None, Level::None, Level::ReadWrite) {
        Err(TrustError::LevelNotOnDimension {
            dimension,
            level,
            allowed,
        }) => {
            assert_eq!(dimension, Dimension::Exec);
            assert_eq!(level, Level::ReadWrite);
            assert!(allowed.contains("Sandboxed"), "allowed was: {allowed}");
        }
        other => panic!("expected LevelNotOnDimension, got {other:?}"),
    }
}
