//! Pure encode/decode tests for the binary canonical-AST format
//! (#206 slice 1). The integration property — "an agent can submit
//! canonical AST without going through the text parser" plus
//! `lex-vcs` StageId stability across the round-trip — lives in
//! `lex-runtime/tests/canonical_format_e2e.rs` since this crate
//! sits upstream of lex-types / lex-bytecode.

use lex_ast::canonical_format::{decode_program, encode_program, CANONICAL_VERSION, DecodeError};
use lex_ast::canonicalize_program;
use lex_syntax::parse_source;

const PROGRAM: &str = r#"
fn add(x :: Int, y :: Int) -> Int { x + y }
fn run() -> Int { add(2, 3) }
"#;

#[test]
fn version_byte_is_first() {
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages);
    assert!(!bytes.is_empty());
    assert_eq!(bytes[0], CANONICAL_VERSION);
    assert_eq!(CANONICAL_VERSION, 1, "v1 is the initial canonical-format version");
}

#[test]
fn encoding_is_deterministic_across_two_parses() {
    // The headline correctness property: two parses of the same
    // `.lex` source produce byte-identical canonical bytes. This is
    // what makes #206's "automatic dedup" story hold — agents
    // proposing the same logical change get the same StageId by
    // construction.
    let a = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let b = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    assert_eq!(encode_program(&a), encode_program(&b));
}

#[test]
fn round_trip_preserves_byte_identity() {
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes_1 = encode_program(&stages);
    let decoded = decode_program(&bytes_1).expect("decode round-trip");
    let bytes_2 = encode_program(&decoded);
    assert_eq!(bytes_1, bytes_2, "encode(decode(encode(s))) must equal encode(s)");
}

#[test]
fn round_trip_preserves_structural_equality() {
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages);
    let decoded = decode_program(&bytes).expect("decode");
    assert_eq!(stages, decoded,
        "decode(encode(s)) must equal s structurally");
}

#[test]
fn empty_input_surfaces_decode_error() {
    let err = decode_program(&[]).unwrap_err();
    assert!(matches!(err, DecodeError::Empty));
}

#[test]
fn unsupported_version_byte_surfaces_clear_error() {
    let mut bytes = encode_program(&canonicalize_program(
        &parse_source(PROGRAM).expect("parse")));
    bytes[0] = 99;
    let err = decode_program(&bytes).unwrap_err();
    match err {
        DecodeError::UnsupportedVersion { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, CANONICAL_VERSION);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn malformed_payload_surfaces_decode_error() {
    let mut bytes = vec![CANONICAL_VERSION];
    bytes.extend_from_slice(b"this isn't JSON");
    let err = decode_program(&bytes).unwrap_err();
    assert!(matches!(err, DecodeError::Deserialize(_)));
}

#[test]
fn lex_vcs_stage_ids_match_across_decode_round_trip() {
    // StageId computation lives in lex-ast, so this test belongs
    // here — pins that the same logical program produces the same
    // StageId after a canonical-AST round-trip. (The full integration
    // through compile_program / typecheck is in
    // lex-runtime/tests/canonical_format_e2e.rs.)
    let stages = canonicalize_program(&parse_source(PROGRAM).expect("parse"));
    let bytes = encode_program(&stages);
    let decoded = decode_program(&bytes).expect("decode");

    let id_pairs: Vec<_> = stages.iter().zip(decoded.iter())
        .filter_map(|(a, b)| {
            let a_id = lex_ast::stage_id(a)?;
            let b_id = lex_ast::stage_id(b)?;
            Some((a_id, b_id))
        })
        .collect();
    assert!(!id_pairs.is_empty(), "expected at least one stage with an id");
    for (a, b) in id_pairs {
        assert_eq!(a, b, "stage ids must match across canonical round-trip");
    }
}
