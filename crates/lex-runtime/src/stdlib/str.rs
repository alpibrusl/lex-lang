//! `std.str` implementations (#778). Semantics and edge cases are stated
//! on the matching `BuiltinDef` in `lex_types::stdlib_spec`; the index
//! convention there (bytes for `len` / `char_at`, codepoints for
//! `slice` / `find` / `find_any`) is checked by `tests/stdlib_table_778.rs`.

use super::Entry;
use crate::builtins::{cp_to_byte, expect_int, expect_list, expect_str, none, remember_cursor, some, str_arg};
use lex_bytecode::Value;

pub(crate) const TABLE: &[Entry] = &[
    ("is_empty", is_empty),
    ("to_int", to_int),
    ("to_float", to_float),
    ("concat", concat),
    ("len", len),
    ("char_at", char_at),
    ("split", split),
    ("join", join),
    ("starts_with", starts_with),
    ("ends_with", ends_with),
    ("contains", contains),
    ("cmp", cmp),
    ("replace", replace),
    ("trim", trim),
    ("to_upper", to_upper),
    ("to_lower", to_lower),
    ("strip_prefix", strip_prefix),
    ("strip_suffix", strip_suffix),
    ("slice", slice),
    ("is_ascii", is_ascii),
    ("find", find),
    ("find_any", find_any),
];

fn is_empty(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Bool(expect_str(args.first())?.is_empty()))
}

/// Length in UTF-8 bytes.
fn len(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Int(expect_str(args.first())?.len() as i64))
}

/// O(1) single-byte access. `str.slice(s, i, i+1)` resolves a codepoint
/// index — O(i) — so scanning a string char-by-char through it is
/// O(n²). `char_at` indexes the UTF-8 bytes directly and returns the
/// byte as a 1-char Str, letting ASCII-oriented scanners (e.g. the JSON
/// parser, whose input is pre-sanitised to single bytes) run in O(n).
/// Returns the char for ASCII bytes (< 128); out-of-range or a
/// non-ASCII byte yields "" — total, never panics.
fn char_at(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let i = expect_int(args.get(1))?;
    if i < 0 {
        return Ok(Value::Str("".into()));
    }
    Ok(match s.as_bytes().get(i as usize) {
        Some(&b) if b < 128 => Value::Str((b as char).to_string().into()),
        _ => Value::Str("".into()),
    })
}

fn concat(args: Vec<Value>) -> Result<Value, String> {
    let a = expect_str(args.first())?;
    let b = expect_str(args.get(1))?;
    Ok(Value::Str(format!("{a}{b}").into()))
}

fn to_int(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    Ok(match s.parse::<i64>() {
        Ok(n) => some(Value::Int(n)),
        Err(_) => none(),
    })
}

fn to_float(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    Ok(match s.parse::<f64>() {
        Ok(f) => some(Value::Float(f)),
        Err(_) => none(),
    })
}

fn split(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let sep = expect_str(args.get(1))?;
    let items: std::collections::VecDeque<Value> = if sep.is_empty() {
        s.chars().map(|c| Value::Str(c.to_string().into())).collect()
    } else {
        s.split(sep).map(|p| Value::Str(p.into())).collect()
    };
    Ok(Value::List(items.into()))
}

fn join(args: Vec<Value>) -> Result<Value, String> {
    let parts = expect_list(args.first())?;
    let sep = expect_str(args.get(1))?;
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        match p {
            Value::Str(s) => out.push_str(s),
            other => return Err(format!("str.join element must be Str, got {other:?}")),
        }
    }
    Ok(Value::Str(out.into()))
}

fn starts_with(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let prefix = expect_str(args.get(1))?;
    Ok(Value::Bool(s.starts_with(prefix)))
}

fn ends_with(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let suffix = expect_str(args.get(1))?;
    Ok(Value::Bool(s.ends_with(suffix)))
}

fn contains(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let needle = expect_str(args.get(1))?;
    Ok(Value::Bool(s.contains(needle)))
}

fn cmp(args: Vec<Value>) -> Result<Value, String> {
    let a = expect_str(args.first())?;
    let b = expect_str(args.get(1))?;
    Ok(Value::Int(match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

fn replace(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let from = expect_str(args.get(1))?;
    let to = expect_str(args.get(2))?;
    Ok(Value::Str(s.replace(from, to).into()))
}

fn trim(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Str(expect_str(args.first())?.trim().into()))
}

fn to_upper(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Str(expect_str(args.first())?.to_uppercase().into()))
}

fn to_lower(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Str(expect_str(args.first())?.to_lowercase().into()))
}

fn strip_prefix(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let prefix = expect_str(args.get(1))?;
    Ok(match s.strip_prefix(prefix) {
        Some(rest) => some(Value::Str(rest.into())),
        None => none(),
    })
}

fn strip_suffix(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let suffix = expect_str(args.get(1))?;
    Ok(match s.strip_suffix(suffix) {
        Some(rest) => some(Value::Str(rest.into())),
        None => none(),
    })
}

/// Half-open codepoint-index slice. `lo` and `hi` are Unicode scalar
/// value (codepoint) indices, not byte offsets. Out-of-range indices
/// clamp to the codepoint count, mirroring Python's `s[lo:hi]`
/// semantics. Reversed ranges error as a caller logic bug. (#620)
///
/// Codepoint indices resolve to byte offsets through the forward-scan
/// cursor (#764). Indices past the end clamp to `s.len()`, yielding an
/// empty slice.
fn slice(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let lo_i = expect_int(args.get(1))?;
    let hi_i = expect_int(args.get(2))?;
    let lo_cp = lo_i.max(0) as usize;
    let hi_cp = hi_i.max(0) as usize;
    if lo_cp > hi_cp {
        return Err(format!("str.slice: reversed range [{lo_cp}..{hi_cp}]"));
    }
    let sv = str_arg(args.first())?;
    let lo_byte = cp_to_byte(sv, lo_cp);
    let hi_byte = cp_to_byte(sv, hi_cp);
    Ok(Value::Str(s[lo_byte..hi_byte].into()))
}

/// One native pass, one VM step. Lets a scanner that needs single-byte
/// input (lex-schema's JSON parser collapses multi-byte chars before
/// parsing) skip its per-char sanitising pass on the common case (#768).
fn is_ascii(args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::Bool(expect_str(args.first())?.is_ascii()))
}

/// Codepoint index of the first occurrence of `needle` at or after
/// codepoint `from`, so a scanner can jump to the next delimiter in one
/// builtin call instead of one `char_at` per character (#764, #768).
/// Indices are codepoint positions, matching `str.slice`. `from` clamps
/// to [0, len]; an empty needle matches at `from`.
fn find(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let needle = expect_str(args.get(1))?;
    let from_cp = expect_int(args.get(2))?.max(0) as usize;
    let sv = str_arg(args.first())?;
    let from_byte = cp_to_byte(sv, from_cp);
    if from_byte >= s.len() && !(from_byte == s.len() && needle.is_empty()) {
        return Ok(none());
    }
    Ok(match s[from_byte..].find(needle) {
        Some(off) => {
            let cp = from_cp + s[from_byte..from_byte + off].chars().count();
            remember_cursor(sv, cp, from_byte + off);
            some(Value::Int(cp as i64))
        }
        None => none(),
    })
}

/// Codepoint index of the first char at or after `from` that occurs in
/// `set`. The JSON-string case: `str.find_any(src, "\"\\", p)` locates
/// the next quote or backslash in one call.
fn find_any(args: Vec<Value>) -> Result<Value, String> {
    let s = expect_str(args.first())?;
    let set = expect_str(args.get(1))?;
    let from_cp = expect_int(args.get(2))?.max(0) as usize;
    let sv = str_arg(args.first())?;
    let from_byte = cp_to_byte(sv, from_cp);
    if from_byte >= s.len() {
        return Ok(none());
    }
    let hit = s[from_byte..]
        .char_indices()
        .enumerate()
        .find(|(_, (_, c))| set.contains(*c));
    Ok(match hit {
        Some((n, (off, _))) => {
            let cp = from_cp + n;
            remember_cursor(sv, cp, from_byte + off);
            some(Value::Int(cp as i64))
        }
        None => none(),
    })
}
