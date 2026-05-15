//! Regression coverage for #417 — `lex fmt` was stripping top-of-file
//! and inter-item line comments because the lexer's logos `skip`
//! directive discarded them before the parser saw them. The parser
//! now scans the gaps between tokens for `#` comments and attaches
//! them to the AST (`Program::leading_comments` for top-of-file,
//! `{Import,TypeDecl,FnDecl}::leading_comments` for per-item).
//!
//! The bug: a leading-docstring block on `lex-schema`'s `src/schema.lex`
//! vanished on `lex fmt`, taking ~140 lines of module documentation
//! with it. These tests pin the round-trip property so any future
//! regression is loud.

use lex_syntax::{parse_source, print_program};

fn fmt(src: &str) -> String {
    let prog = parse_source(src).unwrap_or_else(|e| {
        panic!("parse failed: {e}\nsource:\n{src}")
    });
    print_program(&prog)
}

#[test]
fn top_of_file_doc_block_survives_fmt() {
    // The shape from #417's repro: a multi-line `#` docblock followed
    // by import + fn. Pre-fix the docblock was lopped off entirely.
    let src = "\
# lex-schema — schema introspection + JSON Schema / OpenAPI export
#
# Pydantic's `Model.schema()` returns a JSON Schema describing the
# request/response contract. Lex doesn't have that yet — this module
# is the start.
import \"std.str\" as str

fn id(s :: Str) -> Str { s }
";
    let out = fmt(src);
    for marker in &[
        "# lex-schema — schema introspection + JSON Schema / OpenAPI export",
        "# Pydantic's `Model.schema()` returns a JSON Schema describing the",
        "# request/response contract. Lex doesn't have that yet — this module",
        "# is the start.",
    ] {
        assert!(
            out.contains(marker),
            "leading docblock dropped — missing {marker:?}\noutput:\n{out}"
        );
    }
}

#[test]
fn inter_item_comments_survive_fmt() {
    // Comments between top-level items must also be preserved.
    let src = "\
import \"std.str\" as str

# The main entry. Keep this concise.
fn main() -> Str { \"hi\" }

# Helper: clamps to [0, 100].
fn clamp(n :: Int) -> Int { n }
";
    let out = fmt(src);
    assert!(
        out.contains("# The main entry. Keep this concise."),
        "comment before `main` lost\noutput:\n{out}"
    );
    assert!(
        out.contains("# Helper: clamps to [0, 100]."),
        "comment before `clamp` lost\noutput:\n{out}"
    );
}

#[test]
fn fmt_is_idempotent_with_comments() {
    // Once-through formatting must equal twice-through — the canonical
    // form a project's `lex fmt --check` step depends on.
    let src = "\
# Top docstring.

import \"std.str\" as str

# Above main.
fn main() -> Str { \"hi\" }
";
    let once = fmt(src);
    let twice = fmt(&once);
    assert_eq!(once, twice, "fmt not idempotent:\nonce:\n{once}\ntwice:\n{twice}");
}

#[test]
fn parse_then_print_roundtrips_with_comments() {
    // The full round-trip property: parse, print, re-parse, structural
    // equality. Mirrors the existing round_trip tests but with
    // comments present (which previously vanished, so the test would
    // have trivially passed even on broken behaviour).
    let src = "\
# Module header.
import \"std.str\" as str

# Doc on type.
type Status = Ok | Bad

# Doc on fn.
fn label(s :: Status) -> Str {
  match s {
    Ok  => \"ok\",
    Bad => \"nope\",
  }
}
";
    let prog1 = parse_source(src).expect("parse 1");
    let printed = print_program(&prog1);
    let prog2 = parse_source(&printed).expect("parse 2");
    assert_eq!(prog1, prog2, "round-trip differs.\nprinted:\n{printed}");
}
