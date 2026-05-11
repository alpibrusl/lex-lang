//! Source positions attached to `TypeError`s (#306 slice 1).
//!
//! LLM-driven repair flows need errors that point at a concrete
//! file:line:col, not just a NodeId. `Position` carries that triple;
//! `TypeError` variants gain an `Option<Position>` that the type
//! checker fills in via [`check_program_with_positions`](crate::check_program_with_positions).
//!
//! Slice 1 ships function-level granularity: every error from a
//! given `fn` is stamped with that function's start position. Slice
//! 1.5 will plumb per-expression spans through the canonicalizer
//! so deep-body errors land on the offending sub-expression.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Source file path, when known. `lex check`'s CLI fills this
    /// from the path argument; programmatic callers may leave it
    /// `None` and still benefit from line:col.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number, in chars (not bytes).
    pub col: u32,
}

impl Position {
    pub fn new(file: Option<String>, line: u32, col: u32) -> Self {
        Self { file, line, col }
    }

    /// Render as `file:line:col` (or `line:col` if no file).
    pub fn render(&self) -> String {
        match &self.file {
            Some(f) => format!("{f}:{}:{}", self.line, self.col),
            None => format!("{}:{}", self.line, self.col),
        }
    }
}

/// Translate a byte offset into the source string into a 1-based
/// `(line, col)`. Lines split on `\n`; columns count chars (not
/// bytes), so multi-byte UTF-8 doesn't double-count. Out-of-range
/// offsets clamp to end-of-source.
pub fn byte_to_line_col(src: &str, byte_offset: usize) -> (u32, u32) {
    let cap = byte_offset.min(src.len());
    let mut line: u32 = 1;
    let mut last_line_start = 0usize;
    for (i, b) in src.as_bytes().iter().enumerate().take(cap) {
        if *b == b'\n' {
            line += 1;
            last_line_start = i + 1;
        }
    }
    let col = src[last_line_start..cap].chars().count() as u32 + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_at_start_of_file() {
        assert_eq!(byte_to_line_col("hello", 0), (1, 1));
    }

    #[test]
    fn line_col_after_newline() {
        // "ab\ncd" — offset 3 is 'c' on line 2 col 1.
        assert_eq!(byte_to_line_col("ab\ncd", 3), (2, 1));
    }

    #[test]
    fn line_col_mid_second_line() {
        // "ab\ncde" — offset 5 points at 'e' (c=col 1, d=2, e=3).
        assert_eq!(byte_to_line_col("ab\ncde", 5), (2, 3));
    }

    #[test]
    fn line_col_with_multibyte_chars() {
        // "héllo" — 'é' is 2 bytes; offset past it should still
        // count as col 3 (chars), not col 4 (bytes).
        let s = "héllo";
        let off = s.find('l').unwrap();
        let (line, col) = byte_to_line_col(s, off);
        assert_eq!((line, col), (1, 3));
    }

    #[test]
    fn out_of_range_offset_clamps() {
        let (line, col) = byte_to_line_col("abc", 999);
        assert_eq!((line, col), (1, 4));
    }

    #[test]
    fn position_renders_with_and_without_file() {
        let p = Position::new(Some("hello.lex".into()), 12, 3);
        assert_eq!(p.render(), "hello.lex:12:3");
        let p = Position::new(None, 5, 7);
        assert_eq!(p.render(), "5:7");
    }
}
