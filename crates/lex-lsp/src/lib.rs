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

// ---------------------------------------------------------------
// #304 phase 2a: hover / definition / completion
// ---------------------------------------------------------------

/// One indexed function from a single Lex source — enough to drive
/// hover, definition, and completion without re-running the
/// parser for each request. Built once per source by
/// [`analyze_source`] and cached by the server loop per Uri.
#[derive(Debug, Clone)]
pub struct FnSummary {
    pub name: String,
    /// 0-based LSP position of the `fn` keyword.
    pub def: Position,
    /// One-line render: `(params) -> ret [effects]`.
    pub signature: String,
    /// Declared effect names (excludes `budget` — that's surfaced
    /// separately in [`Self::budget`]).
    pub effects: Vec<String>,
    /// Sum of `[budget(N)]` declarations on the signature, when
    /// any are present.
    pub budget: Option<u64>,
}

/// Per-source analysis cache for hover / definition / completion.
#[derive(Debug, Default, Clone)]
pub struct FileAnalysis {
    pub fns: std::collections::BTreeMap<String, FnSummary>,
    /// Stdlib + path aliases brought into scope (`import "std.io" as io` → `"io"`).
    pub imports: Vec<String>,
}

/// Parse + canonicalise + index the source. Returns `None` when
/// parsing fails (the caller's diagnostics path already shows the
/// parse error; hover/definition/completion just silently
/// degrade until the source parses again).
pub fn analyze_source(src: &str) -> Option<FileAnalysis> {
    let (program, fn_positions) = lex_syntax::parse_source_with_positions(src).ok()?;
    let stages = lex_ast::canonicalize_program(&program);
    let mut fns: std::collections::BTreeMap<String, FnSummary> = Default::default();
    for stage in &stages {
        let lex_ast::Stage::FnDecl(fd) = stage else { continue };
        let signature = render_signature(fd);
        let (effects, budget) = effects_and_budget(&fd.effects);
        let def = match fn_positions.get(&fd.name) {
            Some(&byte) => {
                let (line, col) = lex_types::byte_to_line_col(src, byte);
                Position { line: line.saturating_sub(1), character: col.saturating_sub(1) }
            }
            None => Position { line: 0, character: 0 },
        };
        fns.insert(
            fd.name.clone(),
            FnSummary { name: fd.name.clone(), def, signature, effects, budget },
        );
    }
    let imports: Vec<String> = stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::Import(i) => Some(i.alias.clone()),
            _ => None,
        })
        .collect();
    Some(FileAnalysis { fns, imports })
}

fn render_signature(fd: &lex_ast::FnDecl) -> String {
    let params: Vec<String> = fd
        .params
        .iter()
        .map(|p| format!("{} :: {}", p.name, render_type(&p.ty)))
        .collect();
    let ret = render_type(&fd.return_type);
    let effs = if fd.effects.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = fd
            .effects
            .iter()
            .map(|e| match &e.arg {
                Some(lex_ast::EffectArg::Int { value }) => format!("{}({})", e.name, value),
                Some(lex_ast::EffectArg::Str { value }) => format!("{}({:?})", e.name, value),
                Some(lex_ast::EffectArg::Ident { value }) => format!("{}({})", e.name, value),
                None => e.name.clone(),
            })
            .collect();
        format!(" [{}]", names.join(", "))
    };
    format!("fn {}({}) -> {}{}", fd.name, params.join(", "), ret, effs)
}

fn render_type(t: &lex_ast::TypeExpr) -> String {
    use lex_ast::TypeExpr::*;
    match t {
        Named { name, args } if args.is_empty() => name.clone(),
        Named { name, args } => {
            let inner: Vec<String> = args.iter().map(render_type).collect();
            format!("{}[{}]", name, inner.join(", "))
        }
        Tuple { items } => {
            let inner: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", inner.join(", "))
        }
        Record { fields } => {
            let inner: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: {}", f.name, render_type(&f.ty)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Function { params, ret, .. } => {
            let inner: Vec<String> = params.iter().map(render_type).collect();
            format!("({}) -> {}", inner.join(", "), render_type(ret))
        }
        Union { variants } => {
            let inner: Vec<String> = variants.iter().map(|v| v.name.clone()).collect();
            inner.join(" | ")
        }
        Refined { base, .. } => format!("{}{{...}}", render_type(base)),
    }
}

fn effects_and_budget(effects: &[lex_ast::Effect]) -> (Vec<String>, Option<u64>) {
    let mut budget: u64 = 0;
    let mut had_budget = false;
    let mut other: Vec<String> = Vec::new();
    for e in effects {
        if e.name == "budget" {
            if let Some(lex_ast::EffectArg::Int { value }) = &e.arg {
                budget = budget.saturating_add(*value as u64);
                had_budget = true;
            }
        } else {
            other.push(e.name.clone());
        }
    }
    (other, if had_budget { Some(budget) } else { None })
}

/// Identifier under `pos` in `src`, if any. Matches ASCII alphanumeric
/// + `_`; non-identifier positions return `None`.
pub fn word_at(src: &str, pos: Position) -> Option<String> {
    let byte = position_to_byte(src, pos)?;
    let bytes = src.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut start = byte;
    while start > 0 && is_word(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = byte;
    while end < bytes.len() && is_word(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(std::str::from_utf8(&bytes[start..end]).ok()?.to_string())
}

fn position_to_byte(src: &str, pos: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col: u32 = 0;
    let mut byte: usize = 0;
    for (i, ch) in src.char_indices() {
        if line == pos.line && col == pos.character {
            return Some(i);
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        byte = i + ch.len_utf8();
    }
    if line == pos.line && col == pos.character {
        Some(byte)
    } else {
        None
    }
}

/// Hover content for the symbol at `pos`. Returns `None` when the
/// symbol is unknown to this file (e.g. a parameter binding, a
/// stdlib function, or whitespace).
pub fn hover_at(file: &FileAnalysis, src: &str, pos: Position) -> Option<String> {
    let word = word_at(src, pos)?;
    let f = file.fns.get(&word)?;
    let mut s = format!("```lex\n{}\n```", f.signature);
    if !f.effects.is_empty() {
        s.push_str(&format!("\n\n**effects**: `{}`", f.effects.join(", ")));
    }
    if let Some(b) = f.budget {
        s.push_str(&format!("\n\n**budget**: `{b}`"));
    }
    Some(s)
}

/// Definition position for the symbol at `pos` — points at the
/// `fn` keyword of the matching declaration in the same file.
/// Returns `None` for cross-file or stdlib symbols (queued for
/// phase 2b).
pub fn definition_at(file: &FileAnalysis, src: &str, pos: Position) -> Option<Position> {
    let word = word_at(src, pos)?;
    Some(file.fns.get(&word)?.def)
}

/// Completion candidates: every fn defined in the file plus every
/// import alias. Stdlib module members (e.g. `io.print`) require
/// the stdlib type registry, which is queued for phase 2b.
pub fn completions(file: &FileAnalysis) -> Vec<(String, String, u8)> {
    let mut out: Vec<(String, String, u8)> = Vec::new();
    // (label, detail, kind code per LSP CompletionItemKind:
    //  3 = Function, 9 = Module)
    for f in file.fns.values() {
        out.push((f.name.clone(), f.signature.clone(), 3));
    }
    for alias in &file.imports {
        out.push((alias.clone(), format!("import alias `{alias}`"), 9));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---------------------------------------------------------------
// #304 phase 3a: code actions surfaced from diagnostic suggestions
// ---------------------------------------------------------------

/// One code action surfaced in the editor's lightbulb menu.
///
/// Slice-3a deliverable: the action carries the suggestion's
/// `summary` as `title`, the diagnostic it addresses, and the
/// full suggestion JSON in `data` so an editor extension (or a
/// custom command handler in slice 3b) can pipe it to
/// `lex repair --apply --transform '<json>'`. The actual
/// `WorkspaceEdit` is **not** computed here — that's slice 3b,
/// which needs cursor-to-NodeId mapping plus AST-roundtrip
/// pretty-printing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeActionStub {
    pub title: String,
    pub kind_hint: String,
    pub rule_tag: String,
    pub diagnostic: lsp_types::Diagnostic,
    pub data: serde_json::Value,
}

/// Iterate `diagnostics` and produce a code-action stub for each
/// one whose `data.suggested_transform` is populated. Diagnostics
/// without a static suggestion (e.g. `infinite-type`,
/// `refinement-violation`) yield no actions; the LLM-driven
/// `lex repair --apply` path still works for those.
pub fn code_actions_for_diagnostics(diagnostics: &[lsp_types::Diagnostic]) -> Vec<CodeActionStub> {
    let mut out = Vec::new();
    for d in diagnostics {
        let data = match &d.data {
            Some(v) => v,
            None => continue,
        };
        let sug = match data.get("suggested_transform") {
            Some(s) if s.is_object() => s,
            _ => continue,
        };
        let title = sug
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("apply suggested transform")
            .to_string();
        let kind_hint = sug
            .get("kind_hint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rule_tag = data
            .get("rule_tag")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(CodeActionStub {
            title,
            kind_hint,
            rule_tag,
            diagnostic: d.clone(),
            data: sug.clone(),
        });
    }
    out
}

// ---------------------------------------------------------------
// #304 phase 3b: refactor actions that apply real WorkspaceEdits
// ---------------------------------------------------------------

/// Refactor action surfaced when the cursor is on (or close to) a
/// function whose body is a top-level `let` binding. Selecting
/// the action replaces the file with the inlined-and-re-printed
/// canonical source.
///
/// Phase 3b ships **`InlineLet`** for top-level let-bound fn
/// bodies as the first applying refactor. The other three #280
/// transforms (`RenameLocal`, `ReplaceMatchArm`, `ExtractFunction`)
/// need cursor-to-NodeId mapping that this slice doesn't have
/// yet — top-level let is the one case where the target NodeId
/// is derivable from fn structure alone (`n_0.{n_params + 1}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineLetAction {
    /// `fn` whose body is a top-level let.
    pub fn_name: String,
    /// Binding name in the let — used to render the action title.
    pub let_name: String,
    /// New full-file source after applying `inline_let`. The
    /// server wraps this in a `WorkspaceEdit` that replaces the
    /// whole document.
    pub new_text: String,
}

/// Find every applicable `InlineLet` refactor in `src`, scoped to
/// fns whose declaration line falls inside the requesting LSP
/// `range`. Returns `Vec` so multi-fn files surface multiple
/// actions when a range spans them.
///
/// Phase 3b heuristic: the cursor is "on" a fn when the fn's
/// declaration line is `<= range.end.line` and the next fn's
/// declaration line is `> range.end.line` (or there is no next
/// fn). This is coarse but correct for the dominant single-line-
/// cursor case the lightbulb is built around.
pub fn inline_let_actions(src: &str, range: &Range) -> Vec<InlineLetAction> {
    let (program, fn_positions) = match lex_syntax::parse_source_with_positions(src) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let stages = lex_ast::canonicalize_program(&program);
    let n_stages = stages.len();
    let mut out: Vec<InlineLetAction> = Vec::new();

    // Precompute each fn's declaration line so we can answer
    // "is the cursor in this fn?" without re-tokenising.
    let mut fn_lines: std::collections::BTreeMap<String, u32> = Default::default();
    for (name, byte) in &fn_positions {
        let (line, _col) = lex_types::byte_to_line_col(src, *byte);
        fn_lines.insert(name.clone(), line.saturating_sub(1));
    }

    for (idx, stage) in stages.iter().enumerate() {
        let lex_ast::Stage::FnDecl(fd) = stage else { continue };
        // Bail when the fn body isn't a top-level `Let`.
        let lex_ast::CExpr::Let { name: let_name, .. } = &fd.body else { continue };
        let let_name = let_name.clone();

        // Range scoping: cursor must fall on or past this fn's
        // declaration line and before the next fn's.
        let Some(&this_line) = fn_lines.get(&fd.name) else { continue };
        let next_line = fn_lines
            .values()
            .filter(|&&l| l > this_line)
            .min()
            .copied();
        let cursor = range.end.line;
        if cursor < this_line {
            continue;
        }
        if let Some(n) = next_line {
            if cursor >= n { continue; }
        }

        // Apply the transform. NodeId for the body root is
        // `n_0.{params + 1}`.
        let node_id = lex_ast::NodeId(format!("n_0.{}", fd.params.len() + 1));
        let new_stage = match lex_ast::inline_let(stage, &node_id) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Splice the new stage back in and re-print.
        let mut new_stages: Vec<lex_ast::Stage> = stages.clone();
        new_stages[idx] = new_stage;
        let _ = n_stages; // tracking handle for diagnostics
        let new_text = lex_ast::print_stages(&new_stages);
        out.push(InlineLetAction {
            fn_name: fd.name.clone(),
            let_name,
            new_text,
        });
    }
    out
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
    fn word_at_picks_identifier_under_cursor() {
        let src = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
        // Cursor on the `a` in `add` (line 0, col 3).
        let w = word_at(src, Position { line: 0, character: 4 });
        assert_eq!(w.as_deref(), Some("add"));
    }

    #[test]
    fn word_at_returns_none_on_whitespace() {
        let src = "fn foo() -> Int { 1 }";
        // Cursor on the space before `->`.
        assert_eq!(word_at(src, Position { line: 0, character: 8 }), None);
    }

    #[test]
    fn analyze_source_indexes_fns_with_signatures() {
        let src = "\
import \"std.io\" as io

fn echo(msg :: Str) -> [io, budget(5)] Nil {
    io.print(msg)
}

fn double(n :: Int) -> Int { n + n }
";
        let file = analyze_source(src).expect("parses");
        assert_eq!(file.fns.len(), 2);
        let echo = file.fns.get("echo").expect("echo present");
        assert_eq!(echo.def.line, 2, "echo is on the 3rd line (0-based: 2)");
        assert!(echo.signature.contains("Str"), "sig: {}", echo.signature);
        assert!(echo.effects.contains(&"io".to_string()));
        assert_eq!(echo.budget, Some(5));
        let dbl = file.fns.get("double").unwrap();
        assert_eq!(dbl.effects, Vec::<String>::new());
        assert_eq!(dbl.budget, None);
        assert!(file.imports.contains(&"io".to_string()));
    }

    #[test]
    fn hover_renders_signature_and_effects() {
        let src = "fn echo(msg :: Str) -> [io] Nil { msg }\n";
        let file = analyze_source(src).unwrap();
        // Cursor on `echo` (line 0, col 5).
        let h = hover_at(&file, src, Position { line: 0, character: 5 }).expect("hover");
        assert!(h.contains("fn echo"), "expected sig in hover: {h}");
        assert!(h.contains("**effects**"), "expected effects line: {h}");
    }

    #[test]
    fn definition_jumps_to_fn_keyword() {
        let src = "\
fn double(n :: Int) -> Int { n + n }
fn caller() -> Int { double(2) }
";
        let file = analyze_source(src).unwrap();
        // Cursor on `double` inside `caller`'s body.
        // Line 1 (0-based), word starts around column 21.
        let pos = Position { line: 1, character: 22 };
        let def = definition_at(&file, src, pos).expect("definition");
        assert_eq!(def.line, 0, "`double` is defined on line 0");
        assert_eq!(def.character, 0);
    }

    #[test]
    fn completions_list_fns_and_imports() {
        let src = "\
import \"std.io\" as io
fn foo() -> Int { 1 }
fn bar() -> Int { 2 }
";
        let file = analyze_source(src).unwrap();
        let items = completions(&file);
        let labels: Vec<&str> = items.iter().map(|(l, _, _)| l.as_str()).collect();
        assert!(labels.contains(&"foo"));
        assert!(labels.contains(&"bar"));
        assert!(labels.contains(&"io"), "import alias must appear: {labels:?}");
    }

    #[test]
    fn code_actions_surface_from_suggested_transform() {
        // Build a real diagnostic via the standard pipeline so we
        // exercise the data shape end-to-end (no mocking).
        let src = "fn bad(x :: Int) -> Int { \"oops\" }\n";
        let diags = diagnostics_for_source(src, Some("/tmp/qf.lex"));
        let actions = code_actions_for_diagnostics(&diags);
        assert_eq!(actions.len(), 1, "one action: {actions:?}");
        let a = &actions[0];
        assert_eq!(a.rule_tag, "type-mismatch");
        // type-mismatch maps to ReplaceMatchArm in the static
        // (rule_tag → kind_hint) table from #306 slice 3.
        assert_eq!(a.kind_hint, "ReplaceMatchArm");
        assert!(!a.title.is_empty(), "non-empty title for the action");
    }

    #[test]
    fn diagnostics_without_suggestion_yield_no_action() {
        // Hand-build a Diagnostic whose `data` has no
        // `suggested_transform` field — simulates a rule without
        // a static suggestion (e.g. infinite-type).
        let d = Diagnostic {
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 0, character: 0 },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            code: Some(lsp_types::NumberOrString::String("infinite-type".into())),
            code_description: None,
            source: Some("lex".into()),
            message: "infinite type".into(),
            related_information: None,
            tags: None,
            data: Some(serde_json::json!({
                "rule_tag": "infinite-type",
                "rule_explanation": "Inference would require...",
                "suggested_transform": serde_json::Value::Null,
            })),
        };
        let actions = code_actions_for_diagnostics(&[d]);
        assert!(actions.is_empty());
    }

    #[test]
    fn inline_let_action_round_trips() {
        // Top-level let in the fn body is the slice-3b target.
        // The action's `new_text` re-prints the canonical AST,
        // which folds the let into the body.
        let src = "\
fn one() -> Int {
    let x := 1
    x + x
}
";
        let cursor = Range {
            start: Position { line: 1, character: 0 },
            end: Position { line: 1, character: 0 },
        };
        let actions = inline_let_actions(src, &cursor);
        assert_eq!(actions.len(), 1, "one inline-let action: {actions:?}");
        let a = &actions[0];
        assert_eq!(a.fn_name, "one");
        assert_eq!(a.let_name, "x");
        // The let is gone, replaced by the substituted body.
        assert!(!a.new_text.contains("let x"), "let removed: {}", a.new_text);
        assert!(
            a.new_text.contains("1 + 1") || a.new_text.contains("1+1"),
            "substituted body present: {}",
            a.new_text
        );
    }

    #[test]
    fn inline_let_action_skips_fns_outside_cursor_range() {
        // Two top-level lets in two separate fns. Cursor on the
        // first fn should yield only the first action.
        let src = "\
fn first() -> Int {
    let a := 10
    a
}

fn second() -> Int {
    let b := 20
    b
}
";
        let cursor = Range {
            start: Position { line: 1, character: 0 },
            end: Position { line: 1, character: 0 },
        };
        let actions = inline_let_actions(src, &cursor);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].fn_name, "first");
    }

    #[test]
    fn inline_let_action_skips_fn_without_top_level_let() {
        // No top-level let -> no action surfaced.
        let src = "fn plain(x :: Int) -> Int { x + 1 }\n";
        let cursor = Range {
            start: Position { line: 0, character: 5 },
            end: Position { line: 0, character: 5 },
        };
        assert!(inline_let_actions(src, &cursor).is_empty());
    }

    #[test]
    fn inline_let_action_handles_unparseable_source() {
        // Parse failure must degrade silently (the diagnostics
        // path already surfaces the parse error).
        let src = "fn bad( :: Int { 1 }\n";
        let cursor = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: 0, character: 0 },
        };
        assert!(inline_let_actions(src, &cursor).is_empty());
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
