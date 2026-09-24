# A small, dependency-free syntax highlighter for `.lex` source, written
# in Lex (dogfooding, per md.lex/generate.lex's convention). Used by the
# per-example on-site source pages (`guides/<slug>-src.html`) so a reader
# can see the real file body on doc.lexlang.org itself, not only a link
# out to GitHub's blob view.
#
# This is a hand-written tokenizer grounded in the real lexer
# (crates/lex-syntax/src/token.rs), not a guess at Lex's grammar: the
# keyword list in `is_keyword` is copied from that file's `#[token(...)]`
# list, comments are `#` to end of line (its `skip(r"#[^\n]*")`), and
# string/bytes/interpolated-string literals (`"..."`, `b"..."`, `f"..."`)
# follow its `Str`/`Bytes`/`FStr` regexes. It does not re-implement the
# full grammar (operators, brackets, etc. pass through unhighlighted) —
# "readable and not misleading" is the bar, not a faithful re-lex.
#
# Coverage: keywords, string/bytes/f-string literals, `#` comments,
# capitalized type names (`Str`, `List[Str]`, a custom `type Foo = ...`),
# numeric literals, and — heuristically — bare lowercase identifiers
# that sit directly inside a `[...]` row and are immediately followed by
# `,` or `]` (an effect row's own shape, e.g. `[fs_read, fs_write] Unit`
# or `-> [net] Str`). That heuristic under-highlights some cases (e.g.
# the `[base | E]` row-polymorphism tail has a space, not `,`/`]`,
# right after the name) rather than over-highlighting record field names
# and other unrelated bracketed content — a miss is just unstyled text,
# a false hit would look like a claim about what the code does.
#
# Performance note, learned the hard way on this repo's JSON parser
# (`lex-schema#44`, O(n²) -> O(n)): this walks the source once via
# `str.split(src, "")` (one native pass; each element is already a full
# codepoint, so it stays correct on non-ASCII text like the `—`/`→` an
# example's doc comment might contain) and folds over that list, instead
# of re-`str.slice`-ing the whole string on every character.

import "std.str" as str
import "std.list" as list
import "./md" as md

# ── Character classification ────────────────────────────────────────────
# Membership via `str.contains` on a small fixed alphabet rather than
# byte-range math, so this stays correct regardless of `char_at`'s
# byte-indexed (not codepoint) convention (#47) — these helpers only
# ever see single-codepoint strings from `str.split(src, "")` anyway.

fn is_ident_start(c :: Str) -> Bool {
  str.contains("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_", c)
}

fn is_ident_continue(c :: Str) -> Bool {
  is_ident_start(c) or str.contains("0123456789", c)
}

fn is_digit(c :: Str) -> Bool {
  str.contains("0123456789", c)
}

fn is_upper(c :: Str) -> Bool {
  str.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ", c)
}

# Digits, grouping underscores, a decimal point, hex digits/prefix
# (`0x1F`), binary prefix (`0b1010`), and an exponent marker — plus a
# `+`/`-` immediately after an `e`/`E` (`1e-9`), per token.rs's Float
# regex. Not a strict re-validation of the literal, just "keep
# consuming while this still looks like part of one number".
fn is_number_continue(buf :: Str, c :: Str) -> Bool {
  match str.contains("0123456789_.xXbBaAcCdDeEfF", c) {
    true => true,
    false => match (c == "+") or (c == "-") {
      true => str.ends_with(buf, "e") or str.ends_with(buf, "E"),
      false => false,
    },
  }
}

# Copied from crates/lex-syntax/src/token.rs's `#[token(...)]` keyword
# list. `_` (the standalone discard/wildcard) is included too even
# though the real lexer gives it its own `Underscore` token distinct
# from `Ident` — styling it like a keyword reads better in a `match`.
fn is_keyword(word :: Str) -> Bool {
  match word {
    "fn" => true, "let" => true, "type" => true, "match" => true,
    "if" => true, "else" => true, "return" => true, "import" => true,
    "as" => true, "true" => true, "false" => true, "and" => true,
    "or" => true, "not" => true, "_" => true,
    _ => false,
  }
}

fn span(cls :: Str, text :: Str) -> Str {
  "<span class=\"" + cls + "\">" + md.escape_html(text) + "</span>"
}

fn bump_down(n :: Int) -> Int {
  match n > 0 { true => n - 1, false => 0 }
}

# ── Scan state ───────────────────────────────────────────────────────────
# mode: "code" | "ident" | "number" | "comment" | "string"
#
# `buf` holds the raw (unescaped) text of the token currently being
# scanned, seeded with the character that started it (so a comment's
# `buf` always begins with its `#`, a string's with its opening `"`,
# etc.) `first` remembers an ident/number's first character, so
# classification never needs a second `str.slice` over the token.
# `depth` is the count of unmatched `[` seen so far, for the effect-row
# heuristic. `esc` is true immediately after an unconsumed `\` inside a
# string literal, so the following character can never end the string.

type HlState = {
  out :: Str,
  mode :: Str,
  buf :: Str,
  esc :: Bool,
  first :: Str,
  depth :: Int,
}

fn mk(out :: Str, mode :: Str, buf :: Str, esc :: Bool, first :: Str, depth :: Int) -> HlState {
  { out: out, mode: mode, buf: buf, esc: esc, first: first, depth: depth }
}

fn empty_state() -> HlState {
  mk("", "code", "", false, "", 0)
}

# ── Flushing a completed ident/number token ─────────────────────────────

fn classify_ident(word :: Str, first :: Str, depth :: Int, next_char :: Str) -> Str {
  match is_keyword(word) {
    true => "kw",
    false => match is_upper(first) {
      true => "ty",
      false => match (depth > 0) and ((next_char == ",") or (next_char == "]")) {
        true => "ef",
        false => "",
      },
    },
  }
}

# `next_char` is whatever character just ended the token (already known
# to the caller, since that's exactly what triggered the flush) — used
# only for the effect-row lookahead in `classify_ident`.
fn flush_ident(st :: HlState, next_char :: Str) -> HlState {
  let cls := classify_ident(st.buf, st.first, st.depth, next_char)
  let piece := match cls { "" => md.escape_html(st.buf), _ => span(cls, st.buf) }
  mk(st.out + piece, "code", "", false, "", st.depth)
}

fn flush_number(st :: HlState) -> HlState {
  mk(st.out + span("nu", st.buf), "code", "", false, "", st.depth)
}

# ── Per-mode single-character transitions ───────────────────────────────

fn start_code(st :: HlState, c :: Str) -> HlState {
  match c == "#" {
    true => mk(st.out, "comment", c, false, "", st.depth),
    false => match is_digit(c) {
      true => mk(st.out, "number", c, false, c, st.depth),
      false => match is_ident_start(c) {
        true => mk(st.out, "ident", c, false, c, st.depth),
        false => match c == "\"" {
          true => mk(st.out, "string", c, false, "", st.depth),
          false => match c == "[" {
            true => mk(st.out + md.escape_html(c), "code", "", false, "", st.depth + 1),
            false => match c == "]" {
              true => mk(st.out + md.escape_html(c), "code", "", false, "", bump_down(st.depth)),
              false => mk(st.out + md.escape_html(c), "code", "", false, "", st.depth),
            },
          },
        },
      },
    },
  }
}

fn step_comment(st :: HlState, c :: Str) -> HlState {
  match c == "\n" {
    true => mk(st.out + span("cm", st.buf) + "\n", "code", "", false, "", st.depth),
    false => mk(st.out, "comment", st.buf + c, false, "", st.depth),
  }
}

fn step_string(st :: HlState, c :: Str) -> HlState {
  match st.esc {
    true => mk(st.out, "string", st.buf + c, false, "", st.depth),
    false => match c == "\\" {
      true => mk(st.out, "string", st.buf + c, true, "", st.depth),
      false => match c == "\"" {
        true => mk(st.out + span("st", st.buf + c), "code", "", false, "", st.depth),
        false => mk(st.out, "string", st.buf + c, false, "", st.depth),
      },
    },
  }
}

fn step_ident(st :: HlState, c :: Str) -> HlState {
  match is_ident_continue(c) {
    true => mk(st.out, "ident", st.buf + c, false, st.first, st.depth),
    # `b"..."` / `f"..."` prefixes (token.rs's Bytes/FStr): the prefix is
    # exactly one letter, so this only fires when nothing else has been
    # buffered yet — `b"` starts a bytes literal, `boo"` does not.
    false => match ((st.buf == "b") or (st.buf == "f")) and (c == "\"") {
      true => mk(st.out, "string", st.buf + c, false, "", st.depth),
      false => start_code(flush_ident(st, c), c),
    },
  }
}

fn step_number(st :: HlState, c :: Str) -> HlState {
  match is_number_continue(st.buf, c) {
    true => mk(st.out, "number", st.buf + c, false, st.first, st.depth),
    false => start_code(flush_number(st), c),
  }
}

fn step(st :: HlState, c :: Str) -> HlState {
  match st.mode {
    "comment" => step_comment(st, c),
    "string" => step_string(st, c),
    "ident" => step_ident(st, c),
    "number" => step_number(st, c),
    _ => start_code(st, c),
  }
}

# Defensive close-out for a token still open when the input ends (in
# practice only reachable for malformed/unterminated source, since
# `to_html` pads with a trailing newline and every other mode closes on
# one) — render whatever was buffered instead of dropping it.
fn finish(st :: HlState) -> Str {
  match st.mode {
    "comment" => st.out + span("cm", st.buf),
    "string" => st.out + span("st", st.buf),
    "ident" => (flush_ident(st, "")).out,
    "number" => (flush_number(st)).out,
    _ => st.out,
  }
}

# Public entry point: `.lex` source -> highlighted HTML fragment (inline
# `<span>`s only; the caller wraps the result in `<pre><code>`).
fn to_html(src :: Str) -> Str {
  let padded := match str.ends_with(src, "\n") { true => src, false => src + "\n" }
  let chars := str.split(padded, "")
  let final := list.fold(chars, empty_state(), fn (st :: HlState, c :: Str) -> HlState { step(st, c) })
  finish(final)
}
