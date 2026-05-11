//! M1: lexer, parser, syntax tree, pretty-printer for Lex.
//!
//! See spec §3 for the grammar.

pub mod token;
pub mod syntax;
pub mod parser;
pub mod printer;
pub mod loader;

pub use loader::{load_program, load_program_from_str, LoadError};
pub use parser::{parse, ParseError};
pub use printer::print_program;
pub use syntax::*;
pub use token::{lex, LexError, Token, TokenKind};

/// Convenience: lex + parse a source string.
pub fn parse_source(src: &str) -> Result<Program, SyntaxError> {
    let toks = lex(src).map_err(SyntaxError::Lex)?;
    parse(toks).map_err(SyntaxError::Parse)
}

/// Byte-offset start position of each `fn` declaration, keyed by
/// function name (#306 slice 1). Used by `lex_types::Position`
/// renderers to map a type error back to its `fn` location.
pub type FnPositions = std::collections::BTreeMap<String, usize>;

/// Variant of [`parse_source`] that also returns the byte-offset
/// position of each top-level `fn` declaration in `src`. Used by
/// the `lex check` CLI (and any other LLM-facing tooling) to stamp
/// source positions onto `lex_types::PositionedError`s.
pub fn parse_source_with_positions(src: &str) -> Result<(Program, FnPositions), SyntaxError> {
    let toks = lex(src).map_err(SyntaxError::Lex)?;
    // Capture `fn`-token byte offsets *before* parse consumes them.
    // The token stream preserves source order, so a single linear
    // scan recovering `Fn` → next `Ident` pairs is sufficient. Names
    // collide → last wins; the type checker rejects duplicates
    // upstream so a collision here is structurally impossible.
    let mut fn_positions = FnPositions::new();
    let mut i = 0;
    while i < toks.len() {
        if matches!(toks[i].kind, TokenKind::Fn) {
            let fn_start = toks[i].span.start;
            // Walk forward to the first Ident (skipping newlines).
            let mut j = i + 1;
            while j < toks.len() {
                match &toks[j].kind {
                    TokenKind::Ident(name) => {
                        fn_positions.insert(name.clone(), fn_start);
                        break;
                    }
                    TokenKind::Newline => { j += 1; }
                    _ => break,
                }
            }
        }
        i += 1;
    }
    let program = parse(toks).map_err(SyntaxError::Parse)?;
    Ok((program, fn_positions))
}

#[derive(Debug, thiserror::Error)]
pub enum SyntaxError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error(transparent)]
    Parse(#[from] ParseError),
}
