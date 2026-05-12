//! Language Server Protocol bridge for Lex (#304 phase 1).
//!
//! Pipes `lex_types::check_program` errors to editor inline-error
//! surfaces (VS Code, Cursor, Continue, Zed, JetBrains AI). This
//! crate ships **phase 1** of #304:
//!
//! - `initialize` / `initialized` / `shutdown` lifecycle
//! - `textDocument/didOpen` / `didChange` / `didSave` / `didClose`
//! - `textDocument/publishDiagnostics` with structured errors
//!   carrying `rule_tag` (#306 slice 2) and source positions
//!   (#306 slice 1).
//!
//! Subsequent phases (hover, definition, completion, code actions,
//! repair-hint integration) are queued as follow-up slices.
//!
//! The diagnostic-translation logic in this module is pure
//! (no I/O, no protocol surface), so it's covered by unit tests
//! without needing to spawn a real LSP server.

use lex_types::PositionedError;
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use serde_json::json;

/// Build LSP `Diagnostic`s for a single Lex source string.
///
/// Re-parses + canonicalises + runs `check_program_with_positions`,
/// so callers don't need to know the type-check pipeline. Returns
/// `Vec<Diagnostic>` — empty on a clean type-check.
///
/// `uri_path` is the file system path used when stamping source
/// positions onto diagnostics; pass the URL-decoded path component
/// of the document's `Uri`. Pass `None` if the document is
/// in-memory and has no on-disk file.
pub fn diagnostics_for_source(src: &str, uri_path: Option<&str>) -> Vec<Diagnostic> {
    // Parse failures are surfaced as a single full-document
    // diagnostic since the type checker hasn't run yet. Editor
    // shows a red squiggle on line 1 col 1 with the parser's
    // message; better than silent failure.
    let (program, fn_positions) = match lex_syntax::parse_source_with_positions(src) {
        Ok(pair) => pair,
        Err(e) => {
            return vec![Diagnostic {
                range: full_range(src),
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(lsp_types::NumberOrString::String("parse-error".into())),
                code_description: None,
                source: Some("lex".into()),
                message: format!("{e}"),
                related_information: None,
                tags: None,
                data: None,
            }];
        }
    };

    let stages = lex_ast::canonicalize_program(&program);
    let positions: std::collections::BTreeMap<String, lex_types::Position> = fn_positions
        .into_iter()
        .map(|(name, byte)| {
            let (line, col) = lex_types::byte_to_line_col(src, byte);
            let pos = lex_types::Position::new(uri_path.map(|s| s.to_string()), line, col);
            (name, pos)
        })
        .collect();

    match lex_types::check_program_with_positions(&stages, &positions) {
        Ok(_) => Vec::new(),
        Err(errs) => errs
            .into_iter()
            .map(|e| diagnostic_from_positioned_error(&e, src))
            .collect(),
    }
}

/// Translate one `PositionedError` into an LSP `Diagnostic`.
///
/// - Range: derived from the error's attached `Position` (line+col
///   from #306 slice 1) when present; falls back to a zero-width
///   point at the file start. The diagnostic spans from the
///   reported start to the end of that line — the type checker
///   currently stamps function-level positions, so this paints
///   the whole `fn` line as the offending span. Future slices
///   tighten this to per-expression precision.
/// - Severity: always `ERROR` for type-check failures (Lex has no
///   warning-level type errors today).
/// - Code: the stable `rule_tag` from #306 slice 2 so editors can
///   group / filter / annotate by rule.
/// - Message: the `TypeError`'s `Display` rendering.
/// - Data: serialised `rule_explanation` + `suggested_transform`
///   payload so code-action providers in a future phase can read
///   the hint without re-running the type checker.
pub fn diagnostic_from_positioned_error(e: &PositionedError, src: &str) -> Diagnostic {
    let range = range_from_position(e.position.as_ref(), src);
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp_types::NumberOrString::String(e.error.rule_tag().to_string())),
        code_description: None,
        source: Some("lex".into()),
        message: format!("{}", e.error),
        related_information: None,
        tags: None,
        data: Some(json!({
            "rule_tag": e.error.rule_tag(),
            "rule_explanation": e.error.rule_explanation(),
            "suggested_transform": lex_types::suggested_transform_for(e.error.rule_tag()),
            "at_node": e.error.node(),
        })),
    }
}

fn range_from_position(p: Option<&lex_types::Position>, src: &str) -> Range {
    match p {
        Some(pos) => {
            // LSP `Position` is 0-based; our `Position` is 1-based.
            let line0 = pos.line.saturating_sub(1);
            let col0 = pos.col.saturating_sub(1);
            let start = Position { line: line0, character: col0 };
            // Paint to end-of-line so the squiggle is visible in
            // editors that don't show zero-width markers.
            let end_col = line_length_chars(src, line0);
            let end = Position { line: line0, character: end_col };
            Range { start, end }
        }
        None => Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
        },
    }
}

fn line_length_chars(src: &str, line0: u32) -> u32 {
    src.lines()
        .nth(line0 as usize)
        .map(|l| l.chars().count() as u32)
        .unwrap_or(0)
}

fn full_range(src: &str) -> Range {
    let n_lines = src.lines().count() as u32;
    let last_line_idx = n_lines.saturating_sub(1);
    let last_line_len = line_length_chars(src, last_line_idx);
    Range {
        start: Position { line: 0, character: 0 },
        end: Position { line: last_line_idx, character: last_line_len },
    }
}

/// Lightweight in-memory document registry used by the server
/// loop. Maps `Uri` (as a String — `Uri` itself isn't `Hash`) to
/// the latest known source text. `didOpen` inserts, `didChange`
/// replaces (full-document sync only — incremental sync is queued),
/// `didClose` removes.
#[derive(Default)]
pub struct Documents {
    inner: std::collections::HashMap<String, String>,
}

impl Documents {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, uri: String, text: String) {
        self.inner.insert(uri, text);
    }

    pub fn get(&self, uri: &str) -> Option<&str> {
        self.inner.get(uri).map(|s| s.as_str())
    }

    pub fn remove(&mut self, uri: &str) {
        self.inner.remove(uri);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_program_yields_no_diagnostics() {
        let src = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
        let diags = diagnostics_for_source(src, Some("/tmp/clean.lex"));
        assert!(diags.is_empty(), "expected no diagnostics: {diags:?}");
    }

    #[test]
    fn type_mismatch_surfaces_with_rule_tag() {
        let src = "fn bad(x :: Int) -> Int { \"oops\" }\n";
        let diags = diagnostics_for_source(src, Some("/tmp/bad.lex"));
        assert_eq!(diags.len(), 1, "one diagnostic expected: {diags:?}");
        let d = &diags[0];
        assert_eq!(d.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(d.source.as_deref(), Some("lex"));
        match &d.code {
            Some(lsp_types::NumberOrString::String(tag)) => {
                assert_eq!(tag, "type-mismatch");
            }
            other => panic!("expected stable rule_tag code, got {other:?}"),
        }
        // Range: line 0 col 0 (1-based line 1 col 1 → 0-based 0,0).
        assert_eq!(d.range.start.line, 0);
        assert_eq!(d.range.start.character, 0);
        // Message carries the type checker's prose.
        assert!(d.message.contains("type mismatch"), "got: {}", d.message);
        // Data carries the rule_explanation + suggested_transform.
        let data = d.data.as_ref().expect("data present");
        assert_eq!(data["rule_tag"], "type-mismatch");
        assert!(data["rule_explanation"].as_str().is_some_and(|s| !s.is_empty()));
        // Slice 3 of #306 wired a static suggestion for type-mismatch.
        assert!(data["suggested_transform"].is_object());
    }

    #[test]
    fn second_fn_diagnostic_lands_on_its_own_line() {
        let src = "\
fn first_ok(x :: Int) -> Int { x }
fn broken(x :: Int) -> Str { x }
";
        let diags = diagnostics_for_source(src, Some("/tmp/two.lex"));
        assert_eq!(diags.len(), 1, "one diag for `broken`: {diags:?}");
        // `broken` is on line 2 (1-based) → line 1 in 0-based LSP coords.
        assert_eq!(diags[0].range.start.line, 1);
    }

    #[test]
    fn parse_failure_yields_a_single_full_document_diagnostic() {
        let src = "fn bad( :: Int { 1 }\n"; // missing param name
        let diags = diagnostics_for_source(src, None);
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].code,
            Some(lsp_types::NumberOrString::String("parse-error".into()))
        );
    }

    #[test]
    fn documents_registry_round_trips() {
        let mut docs = Documents::new();
        docs.insert("file:///a.lex".into(), "fn f() -> Int { 1 }".into());
        assert_eq!(docs.get("file:///a.lex"), Some("fn f() -> Int { 1 }"));
        docs.remove("file:///a.lex");
        assert!(docs.get("file:///a.lex").is_none());
    }
}
