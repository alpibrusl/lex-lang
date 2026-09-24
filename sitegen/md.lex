# A small, dependency-free Markdown-lite -> HTML converter, written in
# Lex (dogfooding, per lex-www's generate.lex convention — see #567).
#
# This is NOT a CommonMark implementation. It supports exactly the subset
# actually used by this repo's docs/*.md and the doc comments `lex docs`
# extracts from examples/*.lex: headings (#..######), fenced code blocks
# (```), GitHub-style pipe tables, blockquotes (>), unordered (-) and
# ordered (N.) lists with wrapped continuation lines, horizontal rules,
# and inline `code`, **bold**, *italic*, [text](url). It also recognizes
# a 2-space-indented block as a code block, which is what the
# "Run:\n  lex run ..." style blocks in .lex doc comments use instead of
# fences.
#
# Deliberately not handled: nested block quotes, reference-style links,
# HTML passthrough, footnotes, setext headings. Everything is escaped
# before any tag is introduced, so worst case an unsupported construct
# renders as literal (safe) text instead of broken markup.

import "std.str" as str
import "std.list" as list
import "std.tuple" as tuple

# ── Escaping ─────────────────────────────────────────────────────────────

fn escape_html(s :: Str) -> Str {
  let s1 := str.replace(s, "&", "&amp;")
  let s2 := str.replace(s1, "<", "&lt;")
  let s3 := str.replace(s2, ">", "&gt;")
  s3
}

fn escape_attr(s :: Str) -> Str {
  str.replace(escape_html(s), "\"", "&quot;")
}

# ── Slugify (for heading anchors) ───────────────────────────────────────

fn slugify(s :: Str) -> Str {
  let s1 := str.to_lower(str.trim(s))
  let s2 := str.replace(s1, "`", "")
  let s3 := str.replace(s2, "'", "")
  let s4 := str.replace(s3, "\"", "")
  let s5 := str.replace(s4, "(", "")
  let s6 := str.replace(s5, ")", "")
  let s7 := str.replace(s6, ",", "")
  let s8 := str.replace(s7, ".", "")
  let s9 := str.replace(s8, ":", "")
  let s10 := str.replace(s9, "/", "-")
  let s11 := str.replace(s10, "&", "and")
  let s12 := str.replace(s11, "—", "-")
  let s13 := str.replace(s12, "–", "-")
  let s14 := str.replace(s13, "[", "")
  let s15 := str.replace(s14, "]", "")
  let s16 := str.replace(s15, " ", "-")
  let s17 := str.replace(s16, "--", "-")
  let s18 := str.replace(s17, "--", "-")
  s18
}

# ── Inline rendering ─────────────────────────────────────────────────────
# Order matters: escape first, then `code` (so bold/italic never rewrite
# inside a code span's delimiters), then **bold**, then *italic*, then
# [text](url) last (so link text can already carry the tags above).

# Code spans are extracted to an opaque placeholder *before* bold/italic
# run, not just wrapped in <code> inline. A glob like `src/**/*.lex` or a
# shell `*` wildcard inside backticks has its own literal asterisks; if
# bold/italic scanned the already-<code>-wrapped text, those asterisks
# would pair up with real emphasis markers later in the same paragraph
# and corrupt both. Extracting to a marker string removes the asterisks
# from view entirely until the placeholder is restored at the very end.
# Plain ASCII so its byte length equals its codepoint length — `find`
# returns codepoint indices and `slice` takes codepoint indices, but
# `len` is byte length; mixing those up for a non-ASCII marker would
# misplace the restore. This token isn't valid Markdown syntax anywhere
# else in this renderer, so it can't collide with a real match.
fn code_marker() -> Str { "@@LXDOCCODE@@" }

fn extract_code(s :: Str) -> Tuple[Str, List[Str]] {
  match str.find(s, "`", 0) {
    None => (s, []),
    Some(start) => {
      let after := str.slice(s, start + 1, str.len(s))
      match str.find(after, "`", 0) {
        None => (s, []),
        Some(rel_end) => {
          let before := str.slice(s, 0, start)
          let inner := str.slice(after, 0, rel_end)
          let rest := str.slice(after, rel_end + 1, str.len(after))
          let sub := extract_code(rest)
          (before + code_marker() + tuple.fst(sub), list.cons("<code>" + inner + "</code>", tuple.snd(sub)))
        },
      }
    },
  }
}

fn restore_code(s :: Str, codes :: List[Str]) -> Str {
  match str.find(s, code_marker(), 0) {
    None => s,
    Some(start) => match list.head(codes) {
      None => s,
      Some(c) => {
        let before := str.slice(s, 0, start)
        let after := str.slice(s, start + str.len(code_marker()), str.len(s))
        before + c + restore_code(after, list.tail(codes))
      },
    },
  }
}

fn inline_bold(s :: Str) -> Str {
  match str.find(s, "**", 0) {
    None => s,
    Some(start) => {
      let after := str.slice(s, start + 2, str.len(s))
      match str.find(after, "**", 0) {
        None => s,
        Some(rel_end) => {
          let before := str.slice(s, 0, start)
          let inner := str.slice(after, 0, rel_end)
          let rest := str.slice(after, rel_end + 2, str.len(after))
          before + "<strong>" + inner + "</strong>" + inline_bold(rest)
        },
      }
    },
  }
}

fn inline_italic(s :: Str) -> Str {
  match str.find(s, "*", 0) {
    None => s,
    Some(start) => {
      let after := str.slice(s, start + 1, str.len(s))
      match str.find(after, "*", 0) {
        None => s,
        Some(rel_end) => {
          let before := str.slice(s, 0, start)
          let inner := str.slice(after, 0, rel_end)
          let rest := str.slice(after, rel_end + 1, str.len(after))
          match str.is_empty(inner) {
            # An empty pair (`**`, or a `*` immediately followed by
            # another `*` as part of a larger run, e.g. the glob
            # `src/**/*.lex`) isn't emphasis — leave both markers
            # literal and resume scanning strictly after the second
            # one. Recursing on `after` here (instead of `rest`) would
            # re-offer that same second `*` as a fresh opening marker
            # and pair it with the next `*` in the string, which is
            # exactly the `src/*<em>/</em>.lex` corruption this guards.
            true => before + "**" + inline_italic(rest),
            false => before + "<em>" + inner + "</em>" + inline_italic(rest),
          }
        },
      }
    },
  }
}

fn inline_link(s :: Str) -> Str {
  match str.find(s, "[", 0) {
    None => s,
    Some(start) => {
      let after_open := str.slice(s, start + 1, str.len(s))
      match str.find(after_open, "]", 0) {
        None => s,
        Some(close_rel) => {
          let text := str.slice(after_open, 0, close_rel)
          let after_text := str.slice(after_open, close_rel + 1, str.len(after_open))
          match str.starts_with(after_text, "(") {
            false => str.slice(s, 0, start + 1) + inline_link(after_open),
            true => {
              let after_paren := str.slice(after_text, 1, str.len(after_text))
              match str.find(after_paren, ")", 0) {
                None => s,
                Some(url_end) => {
                  let url := str.slice(after_paren, 0, url_end)
                  let rest := str.slice(after_paren, url_end + 1, str.len(after_paren))
                  let before := str.slice(s, 0, start)
                  before + "<a href=\"" + url + "\">" + text + "</a>" + inline_link(rest)
                },
              }
            },
          }
        },
      }
    },
  }
}

fn render_inline(raw :: Str) -> Str {
  let escaped := escape_html(raw)
  let extracted := extract_code(escaped)
  let protected := tuple.fst(extracted)
  let codes := tuple.snd(extracted)
  let formatted := inline_link(inline_italic(inline_bold(protected)))
  restore_code(formatted, codes)
}

# ── Block-level classification helpers ──────────────────────────────────

fn is_blank(line :: Str) -> Bool {
  str.is_empty(str.trim(line))
}

fn is_fence(line :: Str) -> Bool {
  str.starts_with(str.trim(line), "```")
}

fn is_hr(line :: Str) -> Bool {
  let t := str.trim(line)
  (t == "---") or ((t == "***") or (t == "___"))
}

fn is_quote(line :: Str) -> Bool {
  let t := str.trim(line)
  str.starts_with(t, "> ") or (t == ">")
}

fn quote_text(line :: Str) -> Str {
  let t := str.trim(line)
  match str.strip_prefix(t, "> ") {
    Some(r) => r,
    None => match str.strip_prefix(t, ">") { Some(r) => r, None => t },
  }
}

fn heading_level(line :: Str) -> Int {
  let t := str.trim(line)
  match str.starts_with(t, "###### ") { true => 6, false =>
  match str.starts_with(t, "##### ")  { true => 5, false =>
  match str.starts_with(t, "#### ")   { true => 4, false =>
  match str.starts_with(t, "### ")    { true => 3, false =>
  match str.starts_with(t, "## ")     { true => 2, false =>
  match str.starts_with(t, "# ")      { true => 1, false => 0 } } } } } }
}

fn heading_text(line :: Str, level :: Int) -> Str {
  let t := str.trim(line)
  str.trim(str.slice(t, level + 1, str.len(t)))
}

fn is_ul_item(line :: Str) -> Bool {
  str.starts_with(str.trim(line), "- ")
}

fn ul_item_text(line :: Str) -> Str {
  match str.strip_prefix(str.trim(line), "- ") { Some(r) => r, None => str.trim(line) }
}

fn ol_prefix_len(line :: Str) -> Int {
  let t := str.trim(line)
  match str.starts_with(t, "1. ") { true => 3, false =>
  match str.starts_with(t, "2. ") { true => 3, false =>
  match str.starts_with(t, "3. ") { true => 3, false =>
  match str.starts_with(t, "4. ") { true => 3, false =>
  match str.starts_with(t, "5. ") { true => 3, false =>
  match str.starts_with(t, "6. ") { true => 3, false =>
  match str.starts_with(t, "7. ") { true => 3, false =>
  match str.starts_with(t, "8. ") { true => 3, false =>
  match str.starts_with(t, "9. ") { true => 3, false => 0 } } } } } } } } }
}

fn is_ol_item(line :: Str) -> Bool {
  ol_prefix_len(line) > 0
}

fn ol_item_text(line :: Str) -> Str {
  let t := str.trim(line)
  str.trim(str.slice(t, ol_prefix_len(line), str.len(t)))
}

fn is_indented(line :: Str) -> Bool {
  str.starts_with(line, "  ") and (not is_blank(line))
}

fn is_table_sep(line :: Str) -> Bool {
  let t := str.trim(line)
  let a := str.replace(t, "-", "")
  let b := str.replace(a, "|", "")
  let c := str.replace(b, ":", "")
  let d := str.replace(c, " ", "")
  str.is_empty(d) and str.contains(t, "-")
}

fn is_table_row(line :: Str) -> Bool {
  str.contains(str.trim(line), "|")
}

fn table_cells(line :: Str) -> List[Str] {
  let t := str.trim(line)
  let t1 := match str.strip_prefix(t, "|") { Some(r) => r, None => t }
  let t2 := match str.strip_suffix(t1, "|") { Some(r) => r, None => t1 }
  list.map(str.split(t2, "|"), fn (c :: Str) -> Str { str.trim(c) })
}

fn cells_to_row(cells :: List[Str], tag :: Str) -> Str {
  let inner := list.fold(cells, "", fn (acc :: Str, c :: Str) -> Str {
    acc + "<" + tag + ">" + render_inline(c) + "</" + tag + ">"
  })
  "<tr>" + inner + "</tr>\n"
}

# ── Rendering state ──────────────────────────────────────────────────────
# mode: "none" | "para" | "fence" | "quote" | "ul" | "ol" | "table" | "indent"
#
# Lex has no record-update (`{ r with field: v }`) syntax, so `mk` is the
# one place every field is listed; every state transition below goes
# through it instead of hand-reconstructing seven fields at each call site.

type MdState = {
  out :: Str,
  mode :: Str,
  buf :: Str,
  list_buf :: Str,
  item_buf :: Str,
  table_head :: Str,
  table_body :: Str,
}

fn mk(out :: Str, mode :: Str, buf :: Str, list_buf :: Str, item_buf :: Str, table_head :: Str, table_body :: Str) -> MdState {
  { out: out, mode: mode, buf: buf, list_buf: list_buf, item_buf: item_buf, table_head: table_head, table_body: table_body }
}

fn empty_state() -> MdState {
  mk("", "none", "", "", "", "", "")
}

fn flush_item(st :: MdState) -> MdState {
  match str.is_empty(str.trim(st.item_buf)) {
    true => st,
    false => {
      let li := "<li>" + render_inline(str.trim(st.item_buf)) + "</li>\n"
      mk(st.out, st.mode, st.buf, st.list_buf + li, "", st.table_head, st.table_body)
    },
  }
}

fn flush_block(st :: MdState) -> MdState {
  match st.mode {
    "para" => {
      let text := str.trim(st.buf)
      let out2 := match str.is_empty(text) {
        true => st.out,
        false => st.out + "<p>" + render_inline(text) + "</p>\n",
      }
      mk(out2, "none", "", "", "", "", "")
    },
    "indent" => {
      let out2 := match str.is_empty(str.trim(st.buf)) {
        true => st.out,
        false => st.out + "<pre><code>" + st.buf + "</code></pre>\n",
      }
      mk(out2, "none", "", "", "", "", "")
    },
    "fence" => {
      let out2 := st.out + "<pre><code>" + st.buf + "</code></pre>\n"
      mk(out2, "none", "", "", "", "", "")
    },
    "quote" => {
      let text := str.trim(st.buf)
      let out2 := match str.is_empty(text) {
        true => st.out,
        false => st.out + "<blockquote><p>" + render_inline(text) + "</p></blockquote>\n",
      }
      mk(out2, "none", "", "", "", "", "")
    },
    "ul" => {
      let st2 := flush_item(st)
      let out2 := st.out + "<ul>\n" + st2.list_buf + "</ul>\n"
      mk(out2, "none", "", "", "", "", "")
    },
    "ol" => {
      let st2 := flush_item(st)
      let out2 := st.out + "<ol>\n" + st2.list_buf + "</ol>\n"
      mk(out2, "none", "", "", "", "", "")
    },
    "table" => {
      let out2 := st.out + "<table>\n<thead>" + st.table_head + "</thead>\n<tbody>\n" + st.table_body + "</tbody>\n</table>\n"
      mk(out2, "none", "", "", "", "", "")
    },
    _ => st,
  }
}

fn level_str(n :: Int) -> Str {
  match n { 1 => "1", 2 => "2", 3 => "3", 4 => "4", 5 => "5", 6 => "6", _ => "1" }
}

fn table_start_ahead(rest :: List[Str]) -> Bool {
  match list.head(rest) {
    None => false,
    Some(next) => is_table_sep(next),
  }
}

# Process one line plus the (already-peeked) rest of the lines, returning
# the final rendered HTML once the list is exhausted. `go` always makes
# progress: every branch recurses on a strictly shorter list (or, for the
# table-header case, two lines shorter).
fn go(lines :: List[Str], st :: MdState) -> MdState {
  match list.head(lines) {
    None => flush_block(st),
    Some(line) => {
      let rest := list.tail(lines)
      match st.mode == "fence" {
        true => match is_fence(line) {
          true => go(rest, flush_block(st)),
          false => go(rest, mk(st.out, st.mode, st.buf + escape_html(line) + "\n", st.list_buf, st.item_buf, st.table_head, st.table_body)),
        },
        false => dispatch(line, rest, st),
      }
    },
  }
}

fn dispatch(line :: Str, rest :: List[Str], st :: MdState) -> MdState {
  match is_fence(line) {
    true => {
      let st1 := flush_block(st)
      go(rest, mk(st1.out, "fence", "", "", "", "", ""))
    },
    false => match heading_level(line) > 0 {
      true => {
        let lvl := heading_level(line)
        let text := heading_text(line, lvl)
        let ls := level_str(lvl)
        let h := "<h" + ls + " id=\"" + slugify(text) + "\">" + render_inline(text) + "</h" + ls + ">\n"
        let st1 := flush_block(st)
        go(rest, mk(st1.out + h, "none", "", "", "", "", ""))
      },
      false => match is_hr(line) {
        true => {
          let st1 := flush_block(st)
          go(rest, mk(st1.out + "<hr>\n", "none", "", "", "", "", ""))
        },
        false => match is_blank(line) {
          true => go(rest, flush_block(st)),
          false => match is_quote(line) {
            true => {
              let st1 := match st.mode == "quote" { true => st, false => flush_block(st) }
              go(rest, mk(st1.out, "quote", st1.buf + quote_text(line) + " ", st1.list_buf, st1.item_buf, st1.table_head, st1.table_body))
            },
            false => dispatch_list_or_table(line, rest, st),
          },
        },
      },
    },
  }
}

fn dispatch_list_or_table(line :: Str, rest :: List[Str], st :: MdState) -> MdState {
  match is_ul_item(line) {
    true => {
      let st1 := match st.mode == "ul" { true => flush_item(st), false => flush_block(st) }
      go(rest, mk(st1.out, "ul", "", st1.list_buf, ul_item_text(line), "", ""))
    },
    false => match is_ol_item(line) {
      true => {
        let st1 := match st.mode == "ol" { true => flush_item(st), false => flush_block(st) }
        go(rest, mk(st1.out, "ol", "", st1.list_buf, ol_item_text(line), "", ""))
      },
      false => match (st.mode == "ul") or (st.mode == "ol") {
        true => go(rest, mk(st.out, st.mode, st.buf, st.list_buf, st.item_buf + " " + str.trim(line), st.table_head, st.table_body)),
        false => dispatch_table_or_indent(line, rest, st),
      },
    },
  }
}

fn dispatch_table_or_indent(line :: Str, rest :: List[Str], st :: MdState) -> MdState {
  match (st.mode == "none") and (is_table_row(line) and table_start_ahead(rest)) {
    true => {
      let st1 := flush_block(st)
      let head := cells_to_row(table_cells(line), "th")
      go(list.tail(rest), mk(st1.out, "table", "", "", "", head, ""))
    },
    false => match st.mode == "table" {
      true => match is_table_row(line) {
        true => go(rest, mk(st.out, "table", "", "", "", st.table_head, st.table_body + cells_to_row(table_cells(line), "td"))),
        false => dispatch_para_or_indent(line, rest, flush_block(st)),
      },
      false => dispatch_para_or_indent(line, rest, st),
    },
  }
}

fn dispatch_para_or_indent(line :: Str, rest :: List[Str], st :: MdState) -> MdState {
  match st.mode == "none" {
    true => match is_indented(line) {
      true => go(rest, mk(st.out, "indent", render_inline(str.trim(line)) + "\n", "", "", "", "")),
      false => go(rest, mk(st.out, "para", str.trim(line) + " ", "", "", "", "")),
    },
    false => match st.mode == "indent" {
      true => match is_indented(line) {
        true => go(rest, mk(st.out, "indent", st.buf + render_inline(str.trim(line)) + "\n", "", "", "", "")),
        false => dispatch_para_or_indent(line, rest, flush_block(st)),
      },
      # A `.lex` doc comment commonly reads "Run:\n  lex run ...\n  curl ...",
      # with no blank line before the indented block — unlike a curated
      # docs/*.md file, which always fences real code. Treat an indented
      # line reached mid-paragraph as starting a new indented code block
      # rather than folding it into the flowing paragraph text.
      false => match is_indented(line) {
        true => dispatch_para_or_indent(line, rest, flush_block(st)),
        false => go(rest, mk(st.out, "para", st.buf + str.trim(line) + " ", "", "", "", "")),
      },
    },
  }
}

# Public entry point: Markdown source -> HTML fragment (no <html>/<body>).
fn to_html(src :: Str) -> Str {
  let lines := str.split(src, "\n")
  let st := go(lines, empty_state())
  st.out
}
