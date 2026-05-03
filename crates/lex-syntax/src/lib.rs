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

#[derive(Debug, thiserror::Error)]
pub enum SyntaxError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error(transparent)]
    Parse(#[from] ParseError),
}
