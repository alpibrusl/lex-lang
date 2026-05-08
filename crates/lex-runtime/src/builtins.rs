//! Pure stdlib builtins — string, numeric, list, option, result, json
//! ops dispatched via the same `EffectHandler` interface as effects, but
//! without policy gates (they have no observable side effects).

use lex_bytecode::{MapKey, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

/// Returns Some(...) if `(kind, op)` names a known pure builtin.
/// `None` means "not handled here; fall through to effect dispatch".
pub fn try_pure_builtin(kind: &str, op: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if !is_pure_module(kind) { return None; }
    // `crypto.random` is the lone effectful op in an otherwise-pure
    // module; let the handler dispatch it under the `[random]` effect
    // kind instead of the pure-builtin bypass.
    if (kind, op) == ("crypto", "random") { return None; }
    // Same shape: `datetime.now` is the only effectful op in
    // `std.datetime` (all the parse/format/arithmetic ops are pure).
    if (kind, op) == ("datetime", "now") { return None; }
    // `std.http` is mostly pure (builders + decoders); only the
    // wire ops `send`/`get`/`post` need the [net] effect handler.
    if (kind, "send") == (kind, op) && kind == "http" { return None; }
    if (kind, "get")  == (kind, op) && kind == "http" { return None; }
    if (kind, "post") == (kind, op) && kind == "http" { return None; }
    Some(dispatch(kind, op, args))
}

/// `kind` is one of the known pure module aliases — used by the policy
/// walk to skip pure builtins that programs reference via imports.
pub fn is_pure_module(kind: &str) -> bool {
    matches!(kind, "str" | "int" | "float" | "bool" | "list"
        | "option" | "result" | "tuple" | "json" | "bytes" | "flow" | "math"
        | "map" | "set" | "crypto" | "regex" | "deque" | "datetime" | "http"
        | "toml" | "yaml" | "dotenv" | "csv" | "test" | "random" | "parser")
}

fn dispatch(kind: &str, op: &str, args: &[Value]) -> Result<Value, String> {
    match (kind, op) {
        // -- str --
        ("str", "is_empty") => Ok(Value::Bool(expect_str(args.first())?.is_empty())),
        ("str", "len") => Ok(Value::Int(expect_str(args.first())?.len() as i64)),
        ("str", "concat") => {
            let a = expect_str(args.first())?;
            let b = expect_str(args.get(1))?;
            Ok(Value::Str(format!("{a}{b}")))
        }
        ("str", "to_int") => {
            let s = expect_str(args.first())?;
            match s.parse::<i64>() {
                Ok(n) => Ok(some(Value::Int(n))),
                Err(_) => Ok(none()),
            }
        }
        ("str", "split") => {
            let s = expect_str(args.first())?;
            let sep = expect_str(args.get(1))?;
            let items: Vec<Value> = if sep.is_empty() {
                s.chars().map(|c| Value::Str(c.to_string())).collect()
            } else {
                s.split(sep.as_str()).map(|p| Value::Str(p.to_string())).collect()
            };
            Ok(Value::List(items))
        }
        ("str", "join") => {
            let parts = expect_list(args.first())?;
            let sep = expect_str(args.get(1))?;
            let mut out = String::new();
            for (i, p) in parts.iter().enumerate() {
                if i > 0 { out.push_str(&sep); }
                match p {
                    Value::Str(s) => out.push_str(s),
                    other => return Err(format!("str.join element must be Str, got {other:?}")),
                }
            }
            Ok(Value::Str(out))
        }
        ("str", "starts_with") => {
            let s = expect_str(args.first())?;
            let prefix = expect_str(args.get(1))?;
            Ok(Value::Bool(s.starts_with(prefix.as_str())))
        }
        ("str", "ends_with") => {
            let s = expect_str(args.first())?;
            let suffix = expect_str(args.get(1))?;
            Ok(Value::Bool(s.ends_with(suffix.as_str())))
        }
        ("str", "contains") => {
            let s = expect_str(args.first())?;
            let needle = expect_str(args.get(1))?;
            Ok(Value::Bool(s.contains(needle.as_str())))
        }
        ("str", "replace") => {
            let s = expect_str(args.first())?;
            let from = expect_str(args.get(1))?;
            let to = expect_str(args.get(2))?;
            Ok(Value::Str(s.replace(from.as_str(), to.as_str())))
        }
        ("str", "trim") => Ok(Value::Str(expect_str(args.first())?.trim().to_string())),
        ("str", "to_upper") => Ok(Value::Str(expect_str(args.first())?.to_uppercase())),
        ("str", "to_lower") => Ok(Value::Str(expect_str(args.first())?.to_lowercase())),
        ("str", "strip_prefix") => {
            let s = expect_str(args.first())?;
            let prefix = expect_str(args.get(1))?;
            Ok(match s.strip_prefix(prefix.as_str()) {
                Some(rest) => some(Value::Str(rest.to_string())),
                None => none(),
            })
        }
        ("str", "strip_suffix") => {
            let s = expect_str(args.first())?;
            let suffix = expect_str(args.get(1))?;
            Ok(match s.strip_suffix(suffix.as_str()) {
                Some(rest) => some(Value::Str(rest.to_string())),
                None => none(),
            })
        }
        ("str", "slice") => {
            // Half-open byte-range slice. `hi` is clamped to `s.len()`
            // and a negative `lo` / `hi` clamps to `0`, mirroring
            // Python's `s[lo:hi]` semantics (and matching what
            // production users expect when slicing fixed sizes off
            // a possibly-shorter string — e.g. the first 64 chars
            // of a license header). Reversed ranges (`lo > hi` after
            // clamping) error since that's a caller logic bug. A
            // mid-codepoint `lo` after clamping still errors so
            // silent UTF-8 truncation never sneaks through.
            let s = expect_str(args.first())?;
            let lo_i = expect_int(args.get(1))?;
            let hi_i = expect_int(args.get(2))?;
            let lo = (lo_i.max(0) as usize).min(s.len());
            let hi = (hi_i.max(0) as usize).min(s.len());
            if lo > hi {
                return Err(format!(
                    "str.slice: reversed range [{lo}..{hi}] (after clamping to len {})",
                    s.len()));
            }
            if !s.is_char_boundary(lo) || !s.is_char_boundary(hi) {
                return Err(format!("str.slice: [{lo}..{hi}] not on char boundaries"));
            }
            Ok(Value::Str(s[lo..hi].to_string()))
        }

        // -- int / float --
        ("int", "to_str") => Ok(Value::Str(expect_int(args.first())?.to_string())),
        ("int", "to_float") => Ok(Value::Float(expect_int(args.first())? as f64)),
        ("float", "to_int") => Ok(Value::Int(expect_float(args.first())? as i64)),
        ("float", "to_str") => Ok(Value::Str(expect_float(args.first())?.to_string())),
        ("str", "to_float") => {
            let s = expect_str(args.first())?;
            match s.parse::<f64>() {
                Ok(f) => Ok(some(Value::Float(f))),
                Err(_) => Ok(none()),
            }
        }

        // -- list --
        ("list", "len") => Ok(Value::Int(expect_list(args.first())?.len() as i64)),
        ("list", "is_empty") => Ok(Value::Bool(expect_list(args.first())?.is_empty())),
        ("list", "head") => {
            let xs = expect_list(args.first())?;
            match xs.first() {
                Some(v) => Ok(some(v.clone())),
                None => Ok(none()),
            }
        }
        ("list", "tail") => {
            let xs = expect_list(args.first())?;
            if xs.is_empty() { Ok(Value::List(Vec::new())) }
            else { Ok(Value::List(xs[1..].to_vec())) }
        }
        ("list", "range") => {
            let lo = expect_int(args.first())?;
            let hi = expect_int(args.get(1))?;
            Ok(Value::List((lo..hi).map(Value::Int).collect()))
        }
        ("list", "concat") => {
            let mut out = expect_list(args.first())?.clone();
            out.extend(expect_list(args.get(1))?.iter().cloned());
            Ok(Value::List(out))
        }

        // -- tuple --
        // Per §11.1: fst, snd, third for 2- and 3-tuples. Index out of
        // range is an error rather than a panic so calling `tuple.third`
        // on a 2-tuple is a clean failure instead of a host crash.
        ("tuple", "fst")   => tuple_index(first_arg(args)?, 0),
        ("tuple", "snd")   => tuple_index(first_arg(args)?, 1),
        ("tuple", "third") => tuple_index(first_arg(args)?, 2),
        ("tuple", "len") => match first_arg(args)? {
            Value::Tuple(items) => Ok(Value::Int(items.len() as i64)),
            other => Err(format!("tuple.len: expected Tuple, got {other:?}")),
        },

        // -- option --
        ("option", "unwrap_or") => {
            let opt = first_arg(args)?;
            let default = args.get(1).cloned().unwrap_or(Value::Unit);
            match opt {
                Value::Variant { name, args } if name == "Some" && !args.is_empty() => Ok(args[0].clone()),
                Value::Variant { name, .. } if name == "None" => Ok(default),
                other => Err(format!("option.unwrap_or expected Option, got {other:?}")),
            }
        }
        ("option", "is_some") => match first_arg(args)? {
            Value::Variant { name, .. } => Ok(Value::Bool(name == "Some")),
            other => Err(format!("option.is_some expected Option, got {other:?}")),
        },
        ("option", "is_none") => match first_arg(args)? {
            Value::Variant { name, .. } => Ok(Value::Bool(name == "None")),
            other => Err(format!("option.is_none expected Option, got {other:?}")),
        },

        // -- result --
        ("result", "is_ok") => match first_arg(args)? {
            Value::Variant { name, .. } => Ok(Value::Bool(name == "Ok")),
            other => Err(format!("result.is_ok expected Result, got {other:?}")),
        },
        ("result", "is_err") => match first_arg(args)? {
            Value::Variant { name, .. } => Ok(Value::Bool(name == "Err")),
            other => Err(format!("result.is_err expected Result, got {other:?}")),
        },
        ("result", "unwrap_or") => {
            let res = first_arg(args)?;
            let default = args.get(1).cloned().unwrap_or(Value::Unit);
            match res {
                Value::Variant { name, args } if name == "Ok" && !args.is_empty() => Ok(args[0].clone()),
                Value::Variant { name, .. } if name == "Err" => Ok(default),
                other => Err(format!("result.unwrap_or expected Result, got {other:?}")),
            }
        }

        // -- json --
        ("json", "stringify") => {
            let v = first_arg(args)?;
            Ok(Value::Str(serde_json::to_string(&value_to_json(v)).unwrap_or_default()))
        }
        ("json", "parse") => {
            let s = expect_str(args.first())?;
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(v) => Ok(ok_v(json_to_value(&v))),
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        // Tactical fix for #168: validate required fields before
        // returning Ok. The full type-driven fix (derive `required`
        // from `T` at type-check time) tracked in #168.
        ("json", "parse_strict") => {
            let s = expect_str(args.first())?;
            let required = required_field_names(args.get(1))?;
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(v) => match check_required_fields(&v, &required) {
                    Ok(()) => Ok(ok_v(json_to_value(&v))),
                    Err(e) => Ok(err_v(Value::Str(e))),
                },
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }

        // -- toml (config parser; routes through serde_json::Value
        // so the parsed shape composes with the existing json
        // tooling. Datetimes become RFC 3339 strings — the only
        // info-losing step) --
        ("toml", "parse") => {
            let s = expect_str(args.first())?;
            match toml::from_str::<serde_json::Value>(&s) {
                Ok(mut v) => {
                    unwrap_toml_datetime_markers(&mut v);
                    Ok(ok_v(json_to_value(&v)))
                }
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        // Tactical fix for #168: validate required fields before
        // returning Ok. Caller passes the field names explicitly
        // so the runtime doesn't need T's shape (which lex-bytecode
        // doesn't carry today). Full type-driven fix tracked in #168.
        ("toml", "parse_strict") => {
            let s = expect_str(args.first())?;
            let required = required_field_names(args.get(1))?;
            match toml::from_str::<serde_json::Value>(&s) {
                Ok(mut v) => {
                    unwrap_toml_datetime_markers(&mut v);
                    match check_required_fields(&v, &required) {
                        Ok(()) => Ok(ok_v(json_to_value(&v))),
                        Err(e) => Ok(err_v(Value::Str(e))),
                    }
                }
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        ("toml", "stringify") => {
            let v = first_arg(args)?;
            // serde_json::Value → toml::Value via its serde impls.
            // TOML's grammar is stricter than JSON's (top-level
            // must be a table; no `null`; no mixed-type arrays),
            // so the conversion can fail — surface as Result::Err
            // rather than panic.
            let json = value_to_json(v);
            match toml::to_string(&json) {
                Ok(s)  => Ok(ok_v(Value::Str(s))),
                Err(e) => Ok(err_v(Value::Str(format!("toml.stringify: {e}")))),
            }
        }

        // -- yaml -- mirrors std.toml. Wraps serde_yaml so values
        // map to the same Lex shape as JSON. YAML's Tag/Anchor
        // features are folded out by serde_yaml's deserialize-to-
        // Value path; non-representable shapes (e.g. non-string
        // map keys when stringifying) surface as Result::Err.
        ("yaml", "parse") => {
            let s = expect_str(args.first())?;
            match serde_yaml::from_str::<serde_json::Value>(&s) {
                Ok(v)  => Ok(ok_v(json_to_value(&v))),
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        // Tactical fix for #168 — same shape as toml.parse_strict.
        ("yaml", "parse_strict") => {
            let s = expect_str(args.first())?;
            let required = required_field_names(args.get(1))?;
            match serde_yaml::from_str::<serde_json::Value>(&s) {
                Ok(v) => match check_required_fields(&v, &required) {
                    Ok(()) => Ok(ok_v(json_to_value(&v))),
                    Err(e) => Ok(err_v(Value::Str(e))),
                },
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        ("yaml", "stringify") => {
            let v = first_arg(args)?;
            let json = value_to_json(v);
            match serde_yaml::to_string(&json) {
                Ok(s)  => Ok(ok_v(Value::Str(s))),
                Err(e) => Ok(err_v(Value::Str(format!("yaml.stringify: {e}")))),
            }
        }

        // -- dotenv -- KEY=VALUE pair files. Hand-rolled parser
        // because the dotenvy crate's API is geared at loading
        // into the process env, not parsing-to-data. The grammar
        // we accept: blank lines, `# comment` lines, and
        // `KEY=VALUE` (optional surrounding `"..."` or `'...'`,
        // unescaped). Simple but covers the .env files in the
        // wild that aren't trying to be shell.
        ("dotenv", "parse") => {
            use std::collections::BTreeMap;
            use lex_bytecode::MapKey;
            let s = expect_str(args.first())?;
            match parse_dotenv(&s) {
                Ok(map) => {
                    let mut bt: BTreeMap<MapKey, Value> = BTreeMap::new();
                    for (k, v) in map {
                        bt.insert(MapKey::Str(k), Value::Str(v));
                    }
                    Ok(ok_v(Value::Map(bt)))
                }
                Err(e) => Ok(err_v(Value::Str(e))),
            }
        }

        // -- csv -- rows-as-lists; first row is whatever the file
        // has. The caller decides whether row 0 is a header. We
        // could ship a `parse_with_headers` later that returns a
        // List[Map[Str, Str]]; v1 keeps the surface tight.
        ("csv", "parse") => {
            let s = expect_str(args.first())?;
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .from_reader(s.as_bytes());
            let mut rows: Vec<Value> = Vec::new();
            for r in rdr.records() {
                match r {
                    Ok(rec) => {
                        let row: Vec<Value> = rec.iter()
                            .map(|f| Value::Str(f.to_string()))
                            .collect();
                        rows.push(Value::List(row));
                    }
                    Err(e) => return Ok(err_v(Value::Str(format!("csv.parse: {e}")))),
                }
            }
            Ok(ok_v(Value::List(rows)))
        }
        ("csv", "stringify") => {
            // List[List[Str]] → CSV string. Mixed-type rows are
            // not allowed (CSV is text-only); non-Str cells get
            // stringified via to_json since that's already the
            // convention for `json.stringify` etc.
            let v = first_arg(args)?;
            let rows = match v {
                Value::List(rs) => rs,
                _ => return Ok(err_v(Value::Str("csv.stringify expects List[List[Str]]".into()))),
            };
            let mut out = Vec::new();
            {
                let mut wtr = csv::WriterBuilder::new()
                    .has_headers(false)
                    .from_writer(&mut out);
                for row in rows {
                    let cells = match row {
                        Value::List(cs) => cs,
                        _ => return Ok(err_v(Value::Str("csv.stringify row must be List[Str]".into()))),
                    };
                    let strs: Vec<String> = cells.iter().map(|c| match c {
                        Value::Str(s) => s.clone(),
                        other => serde_json::to_string(&other.to_json())
                            .unwrap_or_else(|_| String::new()),
                    }).collect();
                    if let Err(e) = wtr.write_record(&strs) {
                        return Ok(err_v(Value::Str(format!("csv.stringify: {e}"))));
                    }
                }
                if let Err(e) = wtr.flush() {
                    return Ok(err_v(Value::Str(format!("csv.stringify flush: {e}"))));
                }
            }
            match String::from_utf8(out) {
                Ok(s) => Ok(ok_v(Value::Str(s))),
                Err(e) => Ok(err_v(Value::Str(format!("csv.stringify utf8: {e}")))),
            }
        }

        // -- test -- tiny assertion library. Each helper is pure
        // and returns `Result[Unit, Str]` so tests are themselves
        // functions returning a Result. A suite is a List the user
        // iterates with `list.fold`; no Rust-side Suite/Runner
        // types in v1, so the whole thing is 4 builtins + a few
        // Lex-source helpers callers can copy into their tests/.
        ("test", "assert_eq") => {
            let a = first_arg(args)?;
            let b = args.get(1).ok_or("test.assert_eq: missing second arg")?;
            if a == b {
                Ok(ok_v(Value::Unit))
            } else {
                Ok(err_v(Value::Str(format!("assert_eq: lhs {} != rhs {}",
                    value_to_json(a), value_to_json(b)))))
            }
        }
        ("test", "assert_ne") => {
            let a = first_arg(args)?;
            let b = args.get(1).ok_or("test.assert_ne: missing second arg")?;
            if a != b {
                Ok(ok_v(Value::Unit))
            } else {
                Ok(err_v(Value::Str(format!("assert_ne: both sides are {}",
                    value_to_json(a)))))
            }
        }
        ("test", "assert_true") => {
            match first_arg(args)? {
                Value::Bool(true) => Ok(ok_v(Value::Unit)),
                Value::Bool(false) => Ok(err_v(Value::Str("assert_true: was false".into()))),
                other => Err(format!("test.assert_true expects Bool, got {other:?}")),
            }
        }
        ("test", "assert_false") => {
            match first_arg(args)? {
                Value::Bool(false) => Ok(ok_v(Value::Unit)),
                Value::Bool(true)  => Ok(err_v(Value::Str("assert_false: was true".into()))),
                other => Err(format!("test.assert_false expects Bool, got {other:?}")),
            }
        }

        // -- bytes --
        ("bytes", "len") => {
            let b = expect_bytes(args.first())?;
            Ok(Value::Int(b.len() as i64))
        }
        ("bytes", "eq") => {
            let a = expect_bytes(args.first())?;
            let b = expect_bytes(args.get(1))?;
            Ok(Value::Bool(a == b))
        }
        ("bytes", "from_str") => {
            let s = expect_str(args.first())?;
            Ok(Value::Bytes(s.into_bytes()))
        }
        ("bytes", "to_str") => {
            let b = expect_bytes(args.first())?;
            match String::from_utf8(b.to_vec()) {
                Ok(s) => Ok(ok_v(Value::Str(s))),
                Err(e) => Ok(err_v(Value::Str(format!("{e}")))),
            }
        }
        ("bytes", "slice") => {
            let b = expect_bytes(args.first())?;
            let lo = expect_int(args.get(1))? as usize;
            let hi = expect_int(args.get(2))? as usize;
            if lo > hi || hi > b.len() {
                return Err(format!("bytes.slice: out of range [{lo}..{hi}] of {}", b.len()));
            }
            Ok(Value::Bytes(b[lo..hi].to_vec()))
        }
        ("bytes", "is_empty") => {
            let b = expect_bytes(args.first())?;
            Ok(Value::Bool(b.is_empty()))
        }

        // -- math --
        // Matrices are stored as the F64Array fast-lane variant (a flat
        // row-major Vec<f64> with shape). Lex code treats them as the
        // type alias `Matrix = { rows :: Int, cols :: Int, data ::
        // List[Float] }`; field access is unsupported, so all
        // introspection happens through these helpers.
        ("math", "exp")   => Ok(Value::Float(expect_float(args.first())?.exp())),
        ("math", "log")   => Ok(Value::Float(expect_float(args.first())?.ln())),
        ("math", "log2")  => Ok(Value::Float(expect_float(args.first())?.log2())),
        ("math", "log10") => Ok(Value::Float(expect_float(args.first())?.log10())),
        ("math", "sqrt")  => Ok(Value::Float(expect_float(args.first())?.sqrt())),
        ("math", "abs")   => Ok(Value::Float(expect_float(args.first())?.abs())),
        ("math", "sin")   => Ok(Value::Float(expect_float(args.first())?.sin())),
        ("math", "cos")   => Ok(Value::Float(expect_float(args.first())?.cos())),
        ("math", "tan")   => Ok(Value::Float(expect_float(args.first())?.tan())),
        ("math", "asin")  => Ok(Value::Float(expect_float(args.first())?.asin())),
        ("math", "acos")  => Ok(Value::Float(expect_float(args.first())?.acos())),
        ("math", "atan")  => Ok(Value::Float(expect_float(args.first())?.atan())),
        ("math", "floor") => Ok(Value::Float(expect_float(args.first())?.floor())),
        ("math", "ceil")  => Ok(Value::Float(expect_float(args.first())?.ceil())),
        ("math", "round") => Ok(Value::Float(expect_float(args.first())?.round())),
        ("math", "trunc") => Ok(Value::Float(expect_float(args.first())?.trunc())),
        ("math", "pow") => {
            let a = expect_float(args.first())?;
            let b = expect_float(args.get(1))?;
            Ok(Value::Float(a.powf(b)))
        }
        ("math", "atan2") => {
            let y = expect_float(args.first())?;
            let x = expect_float(args.get(1))?;
            Ok(Value::Float(y.atan2(x)))
        }
        ("math", "min") => {
            let a = expect_float(args.first())?;
            let b = expect_float(args.get(1))?;
            Ok(Value::Float(a.min(b)))
        }
        ("math", "max") => {
            let a = expect_float(args.first())?;
            let b = expect_float(args.get(1))?;
            Ok(Value::Float(a.max(b)))
        }
        ("math", "zeros") => {
            let r = expect_int(args.first())?;
            let c = expect_int(args.get(1))?;
            if r < 0 || c < 0 {
                return Err(format!("math.zeros: negative dim {r}x{c}"));
            }
            let r = r as usize; let c = c as usize;
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data: vec![0.0; r * c] })
        }
        ("math", "ones") => {
            let r = expect_int(args.first())?;
            let c = expect_int(args.get(1))?;
            if r < 0 || c < 0 {
                return Err(format!("math.ones: negative dim {r}x{c}"));
            }
            let r = r as usize; let c = c as usize;
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data: vec![1.0; r * c] })
        }
        ("math", "from_lists") => {
            let rows = expect_list(args.first())?;
            let r = rows.len();
            if r == 0 {
                return Ok(Value::F64Array { rows: 0, cols: 0, data: Vec::new() });
            }
            let first_row = match &rows[0] {
                Value::List(xs) => xs,
                other => return Err(format!("math.from_lists: row 0 not List, got {other:?}")),
            };
            let c = first_row.len();
            let mut data = Vec::with_capacity(r * c);
            for (i, row) in rows.iter().enumerate() {
                let row = match row {
                    Value::List(xs) => xs,
                    other => return Err(format!("math.from_lists: row {i} not List, got {other:?}")),
                };
                if row.len() != c {
                    return Err(format!("math.from_lists: row {i} has {} cols, expected {c}", row.len()));
                }
                for (j, v) in row.iter().enumerate() {
                    let f = match v {
                        Value::Float(f) => *f,
                        Value::Int(n) => *n as f64,
                        other => return Err(format!("math.from_lists: ({i},{j}) not numeric, got {other:?}")),
                    };
                    data.push(f);
                }
            }
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data })
        }
        ("math", "from_flat") => {
            let r = expect_int(args.first())?;
            let c = expect_int(args.get(1))?;
            let xs = expect_list(args.get(2))?;
            if r < 0 || c < 0 {
                return Err(format!("math.from_flat: negative dim {r}x{c}"));
            }
            let r = r as usize; let c = c as usize;
            if xs.len() != r * c {
                return Err(format!("math.from_flat: list len {} != {}*{}", xs.len(), r, c));
            }
            let mut data = Vec::with_capacity(r * c);
            for v in xs {
                data.push(match v {
                    Value::Float(f) => *f,
                    Value::Int(n)   => *n as f64,
                    other => return Err(format!("math.from_flat: non-numeric element {other:?}")),
                });
            }
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data })
        }
        ("math", "rows") => {
            let (r, _, _) = unpack_matrix(first_arg(args)?)?;
            Ok(Value::Int(r as i64))
        }
        ("math", "cols") => {
            let (_, c, _) = unpack_matrix(first_arg(args)?)?;
            Ok(Value::Int(c as i64))
        }
        ("math", "get") => {
            let (r, c, data) = unpack_matrix(first_arg(args)?)?;
            let i = expect_int(args.get(1))? as usize;
            let j = expect_int(args.get(2))? as usize;
            if i >= r || j >= c {
                return Err(format!("math.get: ({i},{j}) out of {r}x{c}"));
            }
            Ok(Value::Float(data[i * c + j]))
        }
        ("math", "to_flat") => {
            let (_, _, data) = unpack_matrix(first_arg(args)?)?;
            Ok(Value::List(data.into_iter().map(Value::Float).collect()))
        }
        ("math", "transpose") => {
            let (r, c, data) = unpack_matrix(first_arg(args)?)?;
            let mut out = vec![0.0; r * c];
            for i in 0..r {
                for j in 0..c {
                    out[j * r + i] = data[i * c + j];
                }
            }
            Ok(Value::F64Array { rows: c as u32, cols: r as u32, data: out })
        }
        ("math", "matmul") => {
            let (m, k1, a) = unpack_matrix(first_arg(args)?)?;
            let (k2, n, b) = unpack_matrix(args.get(1).ok_or("math.matmul: missing arg 1")?)?;
            if k1 != k2 {
                return Err(format!("math.matmul: dim mismatch {m}x{k1} · {k2}x{n}"));
            }
            // Plain triple loop. For the small matrices used in the ML
            // demo (n<200, k<10) this is well under a millisecond and
            // avoids pulling in matrixmultiply for the runtime crate.
            let mut c = vec![0.0; m * n];
            for i in 0..m {
                for kk in 0..k1 {
                    let aik = a[i * k1 + kk];
                    for j in 0..n {
                        c[i * n + j] += aik * b[kk * n + j];
                    }
                }
            }
            Ok(Value::F64Array { rows: m as u32, cols: n as u32, data: c })
        }
        ("math", "scale") => {
            let s = expect_float(args.first())?;
            let (r, c, mut data) = unpack_matrix(args.get(1).ok_or("math.scale: missing arg 1")?)?;
            for x in &mut data { *x *= s; }
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data })
        }
        ("math", "add") | ("math", "sub") => {
            let (ar, ac, a) = unpack_matrix(first_arg(args)?)?;
            let (br, bc, b) = unpack_matrix(args.get(1).ok_or("math.add/sub: missing arg 1")?)?;
            if ar != br || ac != bc {
                return Err(format!("math.{op}: shape mismatch {ar}x{ac} vs {br}x{bc}"));
            }
            let neg = op == "sub";
            let mut out = a;
            for (i, x) in out.iter_mut().enumerate() {
                if neg { *x -= b[i] } else { *x += b[i] }
            }
            Ok(Value::F64Array { rows: ar as u32, cols: ac as u32, data: out })
        }
        ("math", "sigmoid") => {
            let (r, c, mut data) = unpack_matrix(first_arg(args)?)?;
            for x in &mut data { *x = 1.0 / (1.0 + (-*x).exp()); }
            Ok(Value::F64Array { rows: r as u32, cols: c as u32, data })
        }

        // -- map --
        ("map", "new") => Ok(Value::Map(BTreeMap::new())),
        ("map", "size") => Ok(Value::Int(expect_map(args.first())?.len() as i64)),
        ("map", "has") => {
            let m = expect_map(args.first())?;
            let k = MapKey::from_value(args.get(1).ok_or("map.has: missing key")?)?;
            Ok(Value::Bool(m.contains_key(&k)))
        }
        ("map", "get") => {
            let m = expect_map(args.first())?;
            let k = MapKey::from_value(args.get(1).ok_or("map.get: missing key")?)?;
            Ok(match m.get(&k) {
                Some(v) => some(v.clone()),
                None    => none(),
            })
        }
        ("map", "set") => {
            let mut m = expect_map(args.first())?.clone();
            let k = MapKey::from_value(args.get(1).ok_or("map.set: missing key")?)?;
            let v = args.get(2).ok_or("map.set: missing value")?.clone();
            m.insert(k, v);
            Ok(Value::Map(m))
        }
        ("map", "delete") => {
            let mut m = expect_map(args.first())?.clone();
            let k = MapKey::from_value(args.get(1).ok_or("map.delete: missing key")?)?;
            m.remove(&k);
            Ok(Value::Map(m))
        }
        ("map", "keys") => {
            let m = expect_map(args.first())?;
            Ok(Value::List(m.keys().cloned().map(MapKey::into_value).collect()))
        }
        ("map", "values") => {
            let m = expect_map(args.first())?;
            Ok(Value::List(m.values().cloned().collect()))
        }
        ("map", "entries") => {
            let m = expect_map(args.first())?;
            Ok(Value::List(m.iter()
                .map(|(k, v)| Value::Tuple(vec![k.as_value(), v.clone()]))
                .collect()))
        }
        ("map", "from_list") => {
            let pairs = expect_list(args.first())?;
            let mut m = BTreeMap::new();
            for p in pairs {
                let items = match p {
                    Value::Tuple(items) if items.len() == 2 => items,
                    other => return Err(format!(
                        "map.from_list element must be a 2-tuple, got {other:?}")),
                };
                let k = MapKey::from_value(&items[0])?;
                m.insert(k, items[1].clone());
            }
            Ok(Value::Map(m))
        }

        // -- set --
        ("set", "new") => Ok(Value::Set(BTreeSet::new())),
        ("set", "size") => Ok(Value::Int(expect_set(args.first())?.len() as i64)),
        ("set", "has") => {
            let s = expect_set(args.first())?;
            let k = MapKey::from_value(args.get(1).ok_or("set.has: missing element")?)?;
            Ok(Value::Bool(s.contains(&k)))
        }
        ("set", "add") => {
            let mut s = expect_set(args.first())?.clone();
            let k = MapKey::from_value(args.get(1).ok_or("set.add: missing element")?)?;
            s.insert(k);
            Ok(Value::Set(s))
        }
        ("set", "delete") => {
            let mut s = expect_set(args.first())?.clone();
            let k = MapKey::from_value(args.get(1).ok_or("set.delete: missing element")?)?;
            s.remove(&k);
            Ok(Value::Set(s))
        }
        ("set", "to_list") => {
            let s = expect_set(args.first())?;
            Ok(Value::List(s.iter().cloned().map(MapKey::into_value).collect()))
        }
        ("set", "from_list") => {
            let xs = expect_list(args.first())?;
            let mut s = BTreeSet::new();
            for x in xs {
                s.insert(MapKey::from_value(x)?);
            }
            Ok(Value::Set(s))
        }
        ("set", "union") => {
            let a = expect_set(args.first())?;
            let b = expect_set(args.get(1))?;
            Ok(Value::Set(a.union(b).cloned().collect()))
        }
        ("set", "intersect") => {
            let a = expect_set(args.first())?;
            let b = expect_set(args.get(1))?;
            Ok(Value::Set(a.intersection(b).cloned().collect()))
        }
        ("set", "diff") => {
            let a = expect_set(args.first())?;
            let b = expect_set(args.get(1))?;
            Ok(Value::Set(a.difference(b).cloned().collect()))
        }
        ("set", "is_empty") => Ok(Value::Bool(expect_set(args.first())?.is_empty())),
        ("set", "is_subset") => {
            let a = expect_set(args.first())?;
            let b = expect_set(args.get(1))?;
            Ok(Value::Bool(a.is_subset(b)))
        }

        // -- map helpers --
        ("map", "merge") => {
            // b's entries override a's. We construct a new BTreeMap
            // by extending a with b's pairs.
            let a = expect_map(args.first())?.clone();
            let b = expect_map(args.get(1))?;
            let mut out = a;
            for (k, v) in b {
                out.insert(k.clone(), v.clone());
            }
            Ok(Value::Map(out))
        }
        ("map", "is_empty") => Ok(Value::Bool(expect_map(args.first())?.is_empty())),

        // -- deque --
        ("deque", "new") => Ok(Value::Deque(std::collections::VecDeque::new())),
        ("deque", "size") => Ok(Value::Int(expect_deque(args.first())?.len() as i64)),
        ("deque", "is_empty") => Ok(Value::Bool(expect_deque(args.first())?.is_empty())),
        ("deque", "push_back") => {
            let mut d = expect_deque(args.first())?.clone();
            let x = args.get(1).ok_or("deque.push_back: missing value")?.clone();
            d.push_back(x);
            Ok(Value::Deque(d))
        }
        ("deque", "push_front") => {
            let mut d = expect_deque(args.first())?.clone();
            let x = args.get(1).ok_or("deque.push_front: missing value")?.clone();
            d.push_front(x);
            Ok(Value::Deque(d))
        }
        ("deque", "pop_back") => {
            let mut d = expect_deque(args.first())?.clone();
            match d.pop_back() {
                Some(x) => Ok(Value::Variant {
                    name: "Some".into(),
                    args: vec![Value::Tuple(vec![x, Value::Deque(d)])],
                }),
                None => Ok(Value::Variant { name: "None".into(), args: vec![] }),
            }
        }
        ("deque", "pop_front") => {
            let mut d = expect_deque(args.first())?.clone();
            match d.pop_front() {
                Some(x) => Ok(Value::Variant {
                    name: "Some".into(),
                    args: vec![Value::Tuple(vec![x, Value::Deque(d)])],
                }),
                None => Ok(Value::Variant { name: "None".into(), args: vec![] }),
            }
        }
        ("deque", "peek_back") => {
            let d = expect_deque(args.first())?;
            match d.back() {
                Some(x) => Ok(Value::Variant {
                    name: "Some".into(),
                    args: vec![x.clone()],
                }),
                None => Ok(Value::Variant { name: "None".into(), args: vec![] }),
            }
        }
        ("deque", "peek_front") => {
            let d = expect_deque(args.first())?;
            match d.front() {
                Some(x) => Ok(Value::Variant {
                    name: "Some".into(),
                    args: vec![x.clone()],
                }),
                None => Ok(Value::Variant { name: "None".into(), args: vec![] }),
            }
        }
        ("deque", "from_list") => {
            let xs = expect_list(args.first())?;
            Ok(Value::Deque(xs.iter().cloned().collect()))
        }
        ("deque", "to_list") => {
            let d = expect_deque(args.first())?;
            Ok(Value::List(d.iter().cloned().collect()))
        }

        // -- crypto (pure ops; crypto.random is effectful and routes
        // through the handler under [random], see try_pure_builtin) --
        ("crypto", "sha256") => {
            use sha2::{Digest, Sha256};
            let data = expect_bytes(args.first())?;
            let mut h = Sha256::new();
            h.update(data);
            Ok(Value::Bytes(h.finalize().to_vec()))
        }
        ("crypto", "sha512") => {
            use sha2::{Digest, Sha512};
            let data = expect_bytes(args.first())?;
            let mut h = Sha512::new();
            h.update(data);
            Ok(Value::Bytes(h.finalize().to_vec()))
        }
        ("crypto", "md5") => {
            use md5::{Digest, Md5};
            let data = expect_bytes(args.first())?;
            let mut h = Md5::new();
            h.update(data);
            Ok(Value::Bytes(h.finalize().to_vec()))
        }
        ("crypto", "hmac_sha256") => {
            use hmac::{Hmac, KeyInit, Mac};
            type HmacSha256 = Hmac<sha2::Sha256>;
            let key = expect_bytes(args.first())?;
            let data = expect_bytes(args.get(1))?;
            let mut mac = HmacSha256::new_from_slice(key)
                .map_err(|e| format!("hmac_sha256 key: {e}"))?;
            mac.update(data);
            Ok(Value::Bytes(mac.finalize().into_bytes().to_vec()))
        }
        ("crypto", "hmac_sha512") => {
            use hmac::{Hmac, KeyInit, Mac};
            type HmacSha512 = Hmac<sha2::Sha512>;
            let key = expect_bytes(args.first())?;
            let data = expect_bytes(args.get(1))?;
            let mut mac = HmacSha512::new_from_slice(key)
                .map_err(|e| format!("hmac_sha512 key: {e}"))?;
            mac.update(data);
            Ok(Value::Bytes(mac.finalize().into_bytes().to_vec()))
        }
        ("crypto", "base64_encode") => {
            use base64::{Engine, engine::general_purpose::STANDARD};
            let data = expect_bytes(args.first())?;
            Ok(Value::Str(STANDARD.encode(data)))
        }
        ("crypto", "base64_decode") => {
            use base64::{Engine, engine::general_purpose::STANDARD};
            let s = expect_str(args.first())?;
            match STANDARD.decode(s) {
                Ok(b)  => Ok(ok_v(Value::Bytes(b))),
                Err(e) => Ok(err_v(Value::Str(format!("base64: {e}")))),
            }
        }
        ("crypto", "hex_encode") => {
            let data = expect_bytes(args.first())?;
            Ok(Value::Str(hex::encode(data)))
        }
        ("crypto", "hex_decode") => {
            let s = expect_str(args.first())?;
            match hex::decode(s) {
                Ok(b)  => Ok(ok_v(Value::Bytes(b))),
                Err(e) => Ok(err_v(Value::Str(format!("hex: {e}")))),
            }
        }
        ("crypto", "constant_time_eq") => {
            use subtle::ConstantTimeEq;
            let a = expect_bytes(args.first())?;
            let b = expect_bytes(args.get(1))?;
            // `subtle` returns Choice; comparison only meaningful when
            // lengths match. For mismatched lengths return false in
            // constant time (length itself isn't secret, but we want
            // a single comparison shape).
            let eq = if a.len() == b.len() {
                a.ct_eq(b).into()
            } else {
                false
            };
            Ok(Value::Bool(eq))
        }

        // -- random (#219): pure, seeded RNG. Backed by SplitMix64;
        // state is the u64 mixer state stored as a single i64 in
        // `Rng = { state :: Int }`. Threading the Rng through the
        // call site is the user's responsibility — there is no
        // global RNG and therefore no `[random]` effect tag for
        // pure-seeded usage. --
        ("random", "seed") => {
            let s = args.first().ok_or("random.seed: missing arg")?.as_int();
            // Hash the user-supplied seed once before installing it.
            // SplitMix64 is fine when seeded with any u64, but
            // hashing first protects against pathological seeds
            // (e.g., 0) that would make the very first draw zero.
            let mixed = splitmix64(s as u64).0;
            Ok(rng_value(mixed))
        }
        ("random", "int") => {
            let state = rng_decode(args.first())?;
            let lo = args.get(1).ok_or("random.int: missing lo")?.as_int();
            let hi = args.get(2).ok_or("random.int: missing hi")?.as_int();
            if hi < lo {
                return Err(format!(
                    "random.int: hi ({hi}) must be >= lo ({lo})"));
            }
            let span = (hi as i128) - (lo as i128) + 1;
            let (raw, next_state) = splitmix64(state);
            // Reduce uniformly to [lo, hi]. The bias from a plain
            // modulo is at most `(u64::MAX % span) / u64::MAX`,
            // which for any practical span is invisible. Crypto
            // applications should use `crypto.random` instead.
            let drawn = lo as i128 + (raw as u128 % span as u128) as i128;
            Ok(Value::Tuple(vec![
                Value::Int(drawn as i64),
                rng_value(next_state),
            ]))
        }
        ("random", "float") => {
            let state = rng_decode(args.first())?;
            let (raw, next_state) = splitmix64(state);
            // Take the top 53 bits and divide by 2^53 to land in
            // [0.0, 1.0); this is the standard f64 uniform draw.
            let f = ((raw >> 11) as f64) / ((1u64 << 53) as f64);
            Ok(Value::Tuple(vec![Value::Float(f), rng_value(next_state)]))
        }
        ("random", "choose") => {
            let state = rng_decode(args.first())?;
            let xs = match args.get(1) {
                Some(Value::List(xs)) => xs,
                _ => return Err("random.choose: expected List".into()),
            };
            if xs.is_empty() {
                return Ok(Value::Variant {
                    name: "None".into(), args: vec![],
                });
            }
            let (raw, next_state) = splitmix64(state);
            let idx = (raw as usize) % xs.len();
            let pick = xs[idx].clone();
            Ok(Value::Variant {
                name: "Some".into(),
                args: vec![Value::Tuple(vec![pick, rng_value(next_state)])],
            })
        }

        // -- parser (#217): parser combinators. Parser values are
        // tagged Records — `{ kind: "Char", ch: "x" }` etc. — so
        // canonical equality follows from the canonical Record
        // encoding. The interpreter is `parser_run_impl`. --
        ("parser", "char") => {
            let s = expect_str(args.first())?;
            if s.chars().count() != 1 {
                return Err(format!(
                    "parser.char: expected 1-character string, got {s:?}"));
            }
            Ok(parser_node("Char", &[("ch", Value::Str(s))]))
        }
        ("parser", "string") => {
            let s = expect_str(args.first())?;
            Ok(parser_node("String", &[("s", Value::Str(s))]))
        }
        ("parser", "digit") => Ok(parser_node("Digit", &[])),
        ("parser", "alpha") => Ok(parser_node("Alpha", &[])),
        ("parser", "whitespace") => Ok(parser_node("Whitespace", &[])),
        ("parser", "eof") => Ok(parser_node("Eof", &[])),
        ("parser", "seq") => {
            let a = args.first().cloned()
                .ok_or_else(|| "parser.seq: missing first parser".to_string())?;
            let b = args.get(1).cloned()
                .ok_or_else(|| "parser.seq: missing second parser".to_string())?;
            Ok(parser_node("Seq", &[("a", a), ("b", b)]))
        }
        ("parser", "alt") => {
            let a = args.first().cloned()
                .ok_or_else(|| "parser.alt: missing first parser".to_string())?;
            let b = args.get(1).cloned()
                .ok_or_else(|| "parser.alt: missing second parser".to_string())?;
            Ok(parser_node("Alt", &[("a", a), ("b", b)]))
        }
        ("parser", "many") => {
            let p = args.first().cloned()
                .ok_or_else(|| "parser.many: missing inner parser".to_string())?;
            Ok(parser_node("Many", &[("p", p)]))
        }
        ("parser", "optional") => {
            let p = args.first().cloned()
                .ok_or_else(|| "parser.optional: missing inner parser".to_string())?;
            Ok(parser_node("Optional", &[("p", p)]))
        }
        // `parser.map` and `parser.and_then` (#221): closure-bearing
        // combinators. Constructors only — actual closure invocation
        // happens at parser.run time via the Vm-level interpreter.
        ("parser", "map") => {
            let p = args.first().cloned()
                .ok_or_else(|| "parser.map: missing parser".to_string())?;
            let f = args.get(1).cloned()
                .ok_or_else(|| "parser.map: missing closure".to_string())?;
            Ok(parser_node("Map", &[("p", p), ("f", f)]))
        }
        ("parser", "and_then") => {
            let p = args.first().cloned()
                .ok_or_else(|| "parser.and_then: missing parser".to_string())?;
            let f = args.get(1).cloned()
                .ok_or_else(|| "parser.and_then: missing closure".to_string())?;
            Ok(parser_node("AndThen", &[("p", p), ("f", f)]))
        }
        // `parser.run` is handled at the Vm level (lex-bytecode's
        // `Op::EffectCall` intercept) — it needs reentrant Vm access
        // to invoke the closures inside `Map` / `AndThen` nodes. The
        // pure-builtin path doesn't have that, so we deliberately do
        // *not* have a `("parser", "run")` arm here.

        // -- regex (the compiled `Regex` is stored as the pattern
        // string; the runtime caches the actual `regex::Regex` so
        // ops don't re-compile on every call) --
        ("regex", "compile") => {
            let pat = expect_str(args.first())?;
            match get_or_compile_regex(&pat) {
                Ok(_) => Ok(ok_v(Value::Str(pat))),
                Err(e) => Ok(err_v(Value::Str(e))),
            }
        }
        ("regex", "is_match") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.is_match: {e}"))?;
            Ok(Value::Bool(re.is_match(&s)))
        }
        ("regex", "find") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.find: {e}"))?;
            match re.captures(&s) {
                Some(caps) => Ok(Value::Variant {
                    name: "Some".into(),
                    args: vec![match_value(&caps)],
                }),
                None => Ok(Value::Variant { name: "None".into(), args: vec![] }),
            }
        }
        ("regex", "find_all") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.find_all: {e}"))?;
            let items: Vec<Value> = re.captures_iter(&s).map(|caps| match_value(&caps)).collect();
            Ok(Value::List(items))
        }
        ("regex", "replace") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let rep = expect_str(args.get(2))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.replace: {e}"))?;
            Ok(Value::Str(re.replace(&s, rep.as_str()).into_owned()))
        }
        ("regex", "replace_all") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let rep = expect_str(args.get(2))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.replace_all: {e}"))?;
            Ok(Value::Str(re.replace_all(&s, rep.as_str()).into_owned()))
        }
        // -- datetime (pure ops; datetime.now is effectful and routes
        // through the handler under [time]) --
        ("datetime", "parse_iso") => {
            let s = expect_str(args.first())?;
            match chrono::DateTime::parse_from_rfc3339(&s) {
                Ok(dt) => Ok(ok_v(Value::Int(instant_from_chrono(dt)))),
                Err(e) => Ok(err_v(Value::Str(format!("parse_iso: {e}")))),
            }
        }
        ("datetime", "format_iso") => {
            let n = expect_int(args.first())?;
            Ok(Value::Str(format_iso(n)))
        }
        ("datetime", "parse") => {
            let s = expect_str(args.first())?;
            let fmt = expect_str(args.get(1))?;
            match chrono::NaiveDateTime::parse_from_str(&s, &fmt) {
                Ok(naive) => {
                    use chrono::TimeZone;
                    match chrono::Utc.from_local_datetime(&naive).single() {
                        Some(dt) => Ok(ok_v(Value::Int(instant_from_chrono(dt)))),
                        None => Ok(err_v(Value::Str("parse: ambiguous local time".into()))),
                    }
                }
                Err(e) => Ok(err_v(Value::Str(format!("parse: {e}")))),
            }
        }
        ("datetime", "format") => {
            let n = expect_int(args.first())?;
            let fmt = expect_str(args.get(1))?;
            let dt = chrono_from_instant(n);
            Ok(Value::Str(dt.format(&fmt).to_string()))
        }
        ("datetime", "to_components") => {
            let n = expect_int(args.first())?;
            let tz = match parse_tz_arg(args.get(1)) {
                Ok(t) => t,
                Err(e) => return Ok(err_v(Value::Str(e))),
            };
            match resolve_tz_to_components(n, &tz) {
                Ok(rec) => Ok(ok_v(rec)),
                Err(e) => Ok(err_v(Value::Str(e))),
            }
        }
        ("datetime", "from_components") => {
            let rec = match args.first() {
                Some(Value::Record(r)) => r.clone(),
                _ => return Err("from_components: expected DateTime record".into()),
            };
            match instant_from_components(&rec) {
                Ok(n) => Ok(ok_v(Value::Int(n))),
                Err(e) => Ok(err_v(Value::Str(e))),
            }
        }
        ("datetime", "add") => {
            let a = expect_int(args.first())?;
            let d = expect_int(args.get(1))?;
            Ok(Value::Int(a.saturating_add(d)))
        }
        ("datetime", "diff") => {
            let a = expect_int(args.first())?;
            let b = expect_int(args.get(1))?;
            Ok(Value::Int(a.saturating_sub(b)))
        }
        ("datetime", "duration_seconds") => {
            let s = expect_float(args.first())?;
            let nanos = (s * 1_000_000_000.0) as i64;
            Ok(Value::Int(nanos))
        }
        ("datetime", "duration_minutes") => {
            let m = expect_int(args.first())?;
            Ok(Value::Int(m.saturating_mul(60_000_000_000)))
        }
        ("datetime", "duration_days") => {
            let d = expect_int(args.first())?;
            Ok(Value::Int(d.saturating_mul(86_400_000_000_000)))
        }

        ("regex", "split") => {
            let pat = expect_str(args.first())?;
            let s = expect_str(args.get(1))?;
            let re = get_or_compile_regex(&pat).map_err(|e| format!("regex.split: {e}"))?;
            let parts: Vec<Value> = re.split(&s).map(|p| Value::Str(p.to_string())).collect();
            Ok(Value::List(parts))
        }

        // -- http (builders + decoders; wire ops live in the
        // effect handler under `[net]`) --
        ("http", "with_header") => {
            let req = expect_record_pure(args.first())?.clone();
            let k = expect_str(args.get(1))?;
            let v = expect_str(args.get(2))?;
            Ok(Value::Record(http_set_header(req, &k, &v)))
        }
        ("http", "with_auth") => {
            let req = expect_record_pure(args.first())?.clone();
            let scheme = expect_str(args.get(1))?;
            let token = expect_str(args.get(2))?;
            let value = format!("{scheme} {token}");
            Ok(Value::Record(http_set_header(req, "Authorization", &value)))
        }
        ("http", "with_query") => {
            let req = expect_record_pure(args.first())?.clone();
            let params = match args.get(1) {
                Some(Value::Map(m)) => m.clone(),
                Some(other) => return Err(format!(
                    "http.with_query: params must be Map[Str, Str], got {other:?}")),
                None => return Err("http.with_query: missing params argument".into()),
            };
            Ok(Value::Record(http_append_query(req, &params)))
        }
        ("http", "with_timeout_ms") => {
            let req = expect_record_pure(args.first())?.clone();
            let ms = expect_int(args.get(1))?;
            let mut out = req;
            out.insert("timeout_ms".into(), Value::Variant {
                name: "Some".into(),
                args: vec![Value::Int(ms)],
            });
            Ok(Value::Record(out))
        }
        ("http", "json_body") => {
            let resp = expect_record_pure(args.first())?;
            let body = match resp.get("body") {
                Some(Value::Bytes(b)) => b.clone(),
                _ => return Err("http.json_body: HttpResponse.body must be Bytes".into()),
            };
            let s = match std::str::from_utf8(&body) {
                Ok(s) => s,
                Err(e) => return Ok(http_decode_err_pure(format!("body not UTF-8: {e}"))),
            };
            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(j) => Ok(ok_v(Value::from_json(&j))),
                Err(e) => Ok(http_decode_err_pure(format!("json parse: {e}"))),
            }
        }
        ("http", "text_body") => {
            let resp = expect_record_pure(args.first())?;
            let body = match resp.get("body") {
                Some(Value::Bytes(b)) => b.clone(),
                _ => return Err("http.text_body: HttpResponse.body must be Bytes".into()),
            };
            match String::from_utf8(body) {
                Ok(s) => Ok(ok_v(Value::Str(s))),
                Err(e) => Ok(http_decode_err_pure(format!("body not UTF-8: {e}"))),
            }
        }

        _ => Err(format!("unknown pure builtin: {kind}.{op}")),
    }
}

/// Process-wide cache of compiled regexes, keyed by the pattern
/// string. Compilation is the only cost we want to amortize; matching
/// the same `Regex` from multiple threads is safe (`regex::Regex` is
/// `Send + Sync`).
fn regex_cache() -> &'static Mutex<HashMap<String, regex::Regex>> {
    static CACHE: OnceLock<Mutex<HashMap<String, regex::Regex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_compile_regex(pattern: &str) -> Result<regex::Regex, String> {
    let cache = regex_cache();
    {
        let guard = cache.lock().unwrap();
        if let Some(re) = guard.get(pattern) {
            return Ok(re.clone());
        }
    }
    let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let mut guard = cache.lock().unwrap();
    guard.insert(pattern.to_string(), re.clone());
    Ok(re)
}

/// Build a `Match` record value: `{ text, start, end, groups }` where
/// `groups` is the captured groups in order (group 0 is the full match).
/// Missing optional groups become empty strings.
fn match_value(caps: &regex::Captures) -> Value {
    let m0 = caps.get(0).expect("regex match always has group 0");
    let mut rec = indexmap::IndexMap::new();
    rec.insert("text".into(), Value::Str(m0.as_str().to_string()));
    rec.insert("start".into(), Value::Int(m0.start() as i64));
    rec.insert("end".into(), Value::Int(m0.end() as i64));
    let groups: Vec<Value> = (1..caps.len())
        .map(|i| {
            Value::Str(
                caps.get(i)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            )
        })
        .collect();
    rec.insert("groups".into(), Value::List(groups));
    Value::Record(rec)
}

fn expect_map(v: Option<&Value>) -> Result<&BTreeMap<MapKey, Value>, String> {
    match v {
        Some(Value::Map(m)) => Ok(m),
        other => Err(format!("expected Map, got {other:?}")),
    }
}

fn expect_set(v: Option<&Value>) -> Result<&BTreeSet<MapKey>, String> {
    match v {
        Some(Value::Set(s)) => Ok(s),
        other => Err(format!("expected Set, got {other:?}")),
    }
}

/// Unpack any matrix-shaped Value into (rows, cols, flat row-major data).
/// Accepts the F64Array fast lane and the legacy `Record { rows, cols,
/// data: List[Float] }` shape for compatibility with hand-built matrices.
fn unpack_matrix(v: &Value) -> Result<(usize, usize, Vec<f64>), String> {
    if let Value::F64Array { rows, cols, data } = v {
        return Ok((*rows as usize, *cols as usize, data.clone()));
    }
    let rec = match v {
        Value::Record(r) => r,
        other => return Err(format!("expected matrix, got {other:?}")),
    };
    let rows = match rec.get("rows") {
        Some(Value::Int(n)) => *n as usize,
        _ => return Err("matrix: missing/invalid `rows`".into()),
    };
    let cols = match rec.get("cols") {
        Some(Value::Int(n)) => *n as usize,
        _ => return Err("matrix: missing/invalid `cols`".into()),
    };
    let data = match rec.get("data") {
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(match it {
                    Value::Float(f) => *f,
                    Value::Int(n) => *n as f64,
                    other => return Err(format!("matrix data: not numeric, got {other:?}")),
                });
            }
            out
        }
        _ => return Err("matrix: missing/invalid `data`".into()),
    };
    if data.len() != rows * cols {
        return Err(format!("matrix: data len {} != {rows}*{cols}", data.len()));
    }
    Ok((rows, cols, data))
}

fn expect_bytes(v: Option<&Value>) -> Result<&Vec<u8>, String> {
    match v {
        Some(Value::Bytes(b)) => Ok(b),
        Some(other) => Err(format!("expected Bytes, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn first_arg(args: &[Value]) -> Result<&Value, String> {
    args.first().ok_or_else(|| "missing argument".into())
}

fn tuple_index(v: &Value, i: usize) -> Result<Value, String> {
    match v {
        Value::Tuple(items) => items.get(i).cloned()
            .ok_or_else(|| format!("tuple index {i} out of range (len={})", items.len())),
        other => Err(format!("expected Tuple, got {other:?}")),
    }
}

fn expect_str(v: Option<&Value>) -> Result<String, String> {
    match v {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(other) => Err(format!("expected Str, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_int(v: Option<&Value>) -> Result<i64, String> {
    match v {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => Err(format!("expected Int, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_float(v: Option<&Value>) -> Result<f64, String> {
    match v {
        Some(Value::Float(f)) => Ok(*f),
        Some(other) => Err(format!("expected Float, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_list(v: Option<&Value>) -> Result<&Vec<Value>, String> {
    match v {
        Some(Value::List(xs)) => Ok(xs),
        Some(other) => Err(format!("expected List, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_deque(v: Option<&Value>) -> Result<&std::collections::VecDeque<Value>, String> {
    match v {
        Some(Value::Deque(d)) => Ok(d),
        Some(other) => Err(format!("expected Deque, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn some(v: Value) -> Value { Value::Variant { name: "Some".into(), args: vec![v] } }
fn none() -> Value { Value::Variant { name: "None".into(), args: Vec::new() } }
fn ok_v(v: Value) -> Value { Value::Variant { name: "Ok".into(), args: vec![v] } }
fn err_v(v: Value) -> Value { Value::Variant { name: "Err".into(), args: vec![v] } }

// -- std.parser helpers (#217) ----------------------------------------

/// Construct a tagged parser-AST node. The runtime representation is
/// `{ kind: "Char" | "Seq" | ..., ...children }`; the type system
/// treats these as opaque `Parser[T]` so user code can't poke at the
/// fields. Encoding is canonical because `IndexMap` insertion order
/// is stable and we always insert `kind` first.
fn parser_node(kind: &str, fields: &[(&str, Value)]) -> Value {
    let mut r = indexmap::IndexMap::new();
    r.insert("kind".into(), Value::Str(kind.into()));
    for (k, v) in fields {
        r.insert((*k).into(), v.clone());
    }
    Value::Record(r)
}

// `parser.run` interpretation lives in `lex-bytecode::parser_runtime`
// (#221) — it needs reentrant Vm access to invoke closures inside
// `Map` / `AndThen` nodes, which the pure-builtin path doesn't have.

// -- std.random helpers (#219) ----------------------------------------

/// SplitMix64 — single-`u64` state PRNG that is byte-identical
/// across platforms (no float math, no platform-dependent reductions).
/// Returns `(drawn, next_state)`. Constants are the canonical
/// SplitMix64 mixer from the original 2014 paper.
fn splitmix64(state: u64) -> (u64, u64) {
    let next = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = next;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    let z = z ^ (z >> 31);
    (z, next)
}

/// Encode a SplitMix64 state as the user-facing `Rng` value.
/// `Rng = { state :: Int }`; the type-checker treats `Rng` as
/// opaque so users can't poke at the field.
fn rng_value(state: u64) -> Value {
    let mut fields = indexmap::IndexMap::new();
    fields.insert("state".into(), Value::Int(state as i64));
    Value::Record(fields)
}

/// Pull the SplitMix64 state out of a `Value::Record { state }`.
fn rng_decode(v: Option<&Value>) -> Result<u64, String> {
    let rec = match v {
        Some(Value::Record(r)) => r,
        Some(other) => return Err(format!("expected Rng, got {other:?}")),
        None => return Err("missing Rng arg".into()),
    };
    match rec.get("state") {
        Some(Value::Int(n)) => Ok(*n as u64),
        _ => Err("malformed Rng: missing `state :: Int`".into()),
    }
}

// -- helpers for `std.http` builders / decoders --

fn expect_record_pure(v: Option<&Value>) -> Result<&indexmap::IndexMap<String, Value>, String> {
    match v {
        Some(Value::Record(r)) => Ok(r),
        Some(other) => Err(format!("expected Record, got {other:?}")),
        None => Err("missing Record argument".into()),
    }
}

fn http_decode_err_pure(msg: String) -> Value {
    let inner = Value::Variant {
        name: "DecodeError".into(),
        args: vec![Value::Str(msg)],
    };
    err_v(inner)
}

/// Apply or replace a header in an `HttpRequest` record's `headers`
/// field. Header names are normalized to lowercase to match HTTP/1.1
/// case-insensitivity; an existing entry under any casing is
/// overwritten by the new value.
fn http_set_header(
    mut req: indexmap::IndexMap<String, Value>,
    name: &str,
    value: &str,
) -> indexmap::IndexMap<String, Value> {
    use lex_bytecode::MapKey;
    let mut headers = match req.shift_remove("headers") {
        Some(Value::Map(m)) => m,
        _ => std::collections::BTreeMap::new(),
    };
    let key = MapKey::Str(name.to_lowercase());
    // Drop any case variant of the same header name first so casing
    // flips don't accumulate duplicates.
    let lowered = name.to_lowercase();
    headers.retain(|k, _| match k {
        MapKey::Str(s) => s.to_lowercase() != lowered,
        _ => true,
    });
    headers.insert(key, Value::Str(value.to_string()));
    req.insert("headers".into(), Value::Map(headers));
    req
}

/// Append `?k=v&...` (URL-encoded) to the `url` field of an
/// `HttpRequest` record. Existing query string is preserved and
/// extended with `&`. Iteration order is the input map's natural
/// order (`BTreeMap` → sorted by key) so the produced URL is
/// deterministic.
fn http_append_query(
    mut req: indexmap::IndexMap<String, Value>,
    params: &std::collections::BTreeMap<lex_bytecode::MapKey, Value>,
) -> indexmap::IndexMap<String, Value> {
    use lex_bytecode::MapKey;
    let url = match req.get("url") {
        Some(Value::Str(s)) => s.clone(),
        _ => return req,
    };
    let mut pieces = Vec::new();
    for (k, v) in params {
        let kk = match k { MapKey::Str(s) => s.clone(), _ => continue };
        let vv = match v { Value::Str(s) => s.clone(), _ => continue };
        pieces.push(format!("{}={}", url_encode(&kk), url_encode(&vv)));
    }
    if pieces.is_empty() { return req; }
    let sep = if url.contains('?') { '&' } else { '?' };
    let new_url = format!("{url}{sep}{}", pieces.join("&"));
    req.insert("url".into(), Value::Str(new_url));
    req
}

/// Minimal RFC-3986 percent-encode for `application/x-www-form-
/// urlencoded` query values. Pulling in `urlencoding` for one
/// callsite would drag a dep into the runtime; the inline version is
/// short and easy to audit.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn value_to_json(v: &Value) -> serde_json::Value { v.to_json() }

/// The `toml` crate's serde adapter wraps datetimes in a sentinel
/// object `{"$__toml_private_datetime": "<rfc3339>"}` so that the
/// `Datetime` type round-trips through `serde::Value`. For Lex's
/// purposes a plain RFC-3339 string is what we want — callers can
/// then pipe through `datetime.parse_iso` if they need an
/// `Instant`. Walk the tree and replace each wrapper with its
/// inner string, in-place.
fn unwrap_toml_datetime_markers(v: &mut serde_json::Value) {
    use serde_json::Value as J;
    match v {
        J::Object(map) => {
            // Detect single-key marker objects and replace them
            // with their inner string. We have to take care to
            // avoid borrow conflicts.
            if map.len() == 1 {
                if let Some(J::String(s)) = map.get("$__toml_private_datetime") {
                    let s = s.clone();
                    *v = J::String(s);
                    return;
                }
            }
            for (_, child) in map.iter_mut() {
                unwrap_toml_datetime_markers(child);
            }
        }
        J::Array(items) => {
            for item in items.iter_mut() {
                unwrap_toml_datetime_markers(item);
            }
        }
        _ => {}
    }
}

fn json_to_value(v: &serde_json::Value) -> Value { Value::from_json(v) }

/// Extract the `List[Str]` of required field names from the second
/// argument of `*.parse_strict`. The list is allowed to be empty
/// (the parse degenerates to plain `parse`); other shapes are a
/// caller bug rather than a parse error.
fn required_field_names(arg: Option<&Value>) -> Result<Vec<String>, String> {
    let list = expect_list(arg)?;
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        match v {
            Value::Str(s) => out.push(s.clone()),
            other => return Err(format!(
                "parse_strict: required-fields list must contain Str, got {other:?}"
            )),
        }
    }
    Ok(out)
}

/// Verify that `value` is an object containing every entry in
/// `required`. A required entry may be a plain field name (must
/// exist at the top level) or a dotted path (`"project.license"`)
/// which descends through nested objects. Returns a stable,
/// human-readable error listing every missing path so the agent's
/// verifier can surface it directly.
///
/// Tactical fix for #168 — gives users a way to make `parse[T]`
/// errors propagate as `Result::Err` instead of as runtime
/// `GetField` errors at access time. The full type-driven fix
/// (deriving `required` from `T` at type-check time so plain
/// `parse[T]` works, including auto-wrapping `Option[F]` fields
/// as not-required) is the cleaner endgame; see #168.
///
/// Path semantics:
/// * `"name"` → top-level `name` must be present (any value).
/// * `"a.b.c"` → walk `a`, then `b`, then check `c` exists. Each
///   intermediate value must itself be an object.
/// * `\\.` is the literal-dot escape (e.g. `"weird\\.key"` for a
///   field that genuinely contains a dot in its name).
fn check_required_fields(
    value: &serde_json::Value,
    required: &[String],
) -> Result<(), String> {
    if required.is_empty() {
        return Ok(());
    }
    if !matches!(value, serde_json::Value::Object(_)) {
        return Err(format!(
            "parse_strict: expected top-level object with fields {:?}, got {value}",
            required
        ));
    }
    let mut missing: Vec<String> = Vec::new();
    for path in required {
        if !path_exists(value, path) {
            missing.push(path.clone());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing required field(s): {}", missing.join(", ")))
    }
}

/// Walk `value` along the dotted `path` and report whether the
/// terminal segment exists. Intermediate non-object stops surface
/// as "missing" — a path can't traverse through a string, list, or
/// scalar.
fn path_exists(value: &serde_json::Value, path: &str) -> bool {
    let mut cursor = value;
    let segments = split_dotted_path(path);
    for seg in &segments {
        match cursor {
            serde_json::Value::Object(o) => match o.get(seg.as_str()) {
                Some(next) => cursor = next,
                None => return false,
            },
            _ => return false,
        }
    }
    true
}

/// Split `"a.b.c"` into `["a", "b", "c"]`, with `\.` recognised
/// as a literal-dot escape so legitimate dotted field names
/// (e.g. `"package\.json"`) don't accidentally start a descent.
fn split_dotted_path(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut iter = path.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\\' {
            // Backslash at end is preserved; only `\.` is special.
            if let Some(&'.') = iter.peek() {
                cur.push('.');
                iter.next();
                continue;
            }
            cur.push(c);
        } else if c == '.' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// Parse a `.env`-style file into key→value pairs. Accepts:
///
/// * Blank lines and `# comment` lines (ignored).
/// * `KEY=VALUE` with no spaces around `=`. Optional surrounding
///   `"..."` or `'...'` quotes on the value. No escape sequences,
///   no shell expansion — by design; we want this to be a *data*
///   parser, not a shell snippet evaluator.
///
/// Errors carry the offending line number (1-indexed) so the
/// agent's verifier can point a human at the right place.
fn parse_dotenv(src: &str) -> Result<indexmap::IndexMap<String, String>, String> {
    let mut out = indexmap::IndexMap::new();
    for (idx, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Optional `export KEY=VALUE` shell form — accepted for
        // compat with files that grew out of `set -a` workflows.
        let after_export = line.strip_prefix("export ").unwrap_or(line);
        let (k, v) = match after_export.split_once('=') {
            Some(kv) => kv,
            None => return Err(format!("dotenv.parse line {}: missing `=`", idx + 1)),
        };
        let key = k.trim();
        if key.is_empty() {
            return Err(format!("dotenv.parse line {}: empty key", idx + 1));
        }
        let v_trim = v.trim();
        let value = if let Some(q) = v_trim.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            q.to_string()
        } else if let Some(q) = v_trim.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            q.to_string()
        } else {
            v_trim.to_string()
        };
        out.insert(key.to_string(), value);
    }
    Ok(out)
}

// -- datetime helpers (Instant ↔ chrono::DateTime<Utc>) --

/// Convert a `chrono::DateTime` (any `TimeZone`) into a Lex `Instant`,
/// represented as nanoseconds since the UTC unix epoch. Saturates on
/// out-of-range timestamps so the runtime never panics.
fn instant_from_chrono<Tz: chrono::TimeZone>(dt: chrono::DateTime<Tz>) -> i64 {
    dt.timestamp_nanos_opt().unwrap_or(i64::MAX)
}

fn chrono_from_instant(n: i64) -> chrono::DateTime<chrono::Utc> {
    let secs = n.div_euclid(1_000_000_000);
    let nanos = n.rem_euclid(1_000_000_000) as u32;
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_opt(secs, nanos)
        .single()
        .unwrap_or_else(chrono::Utc::now)
}

fn format_iso(n: i64) -> String {
    chrono_from_instant(n).to_rfc3339()
}

/// Parsed form of the user-side `Tz` variant. Mirrors the type
/// registered in `TypeEnv::new_with_builtins`.
enum TzArg {
    Utc,
    Local,
    /// Fixed offset in minutes east of UTC.
    Offset(i32),
    /// IANA name like `"America/New_York"`.
    Iana(String),
}

fn parse_tz_arg(v: Option<&Value>) -> Result<TzArg, String> {
    match v {
        Some(Value::Variant { name, args }) => match (name.as_str(), args.as_slice()) {
            ("Utc", []) => Ok(TzArg::Utc),
            ("Local", []) => Ok(TzArg::Local),
            ("Offset", [Value::Int(m)]) => {
                let m = i32::try_from(*m).map_err(|_| {
                    format!("Tz::Offset: minutes out of range: {m}")
                })?;
                Ok(TzArg::Offset(m))
            }
            ("Iana", [Value::Str(s)]) => Ok(TzArg::Iana(s.clone())),
            (other, _) => Err(format!(
                "expected Tz variant (Utc | Local | Offset(Int) | Iana(Str)), got `{other}` with {} arg(s)",
                args.len()
            )),
        },
        Some(other) => Err(format!("expected Tz variant, got {other:?}")),
        None => Err("missing Tz argument".into()),
    }
}

fn resolve_tz_to_components(n: i64, tz: &TzArg) -> Result<Value, String> {
    use chrono::{TimeZone, Datelike, Timelike, Offset};
    let utc_dt = chrono_from_instant(n);
    let (y, m, d, hh, mm, ss, ns, off_min) = match tz {
        TzArg::Utc => {
            let d = utc_dt;
            (d.year(), d.month() as i32, d.day() as i32,
             d.hour() as i32, d.minute() as i32, d.second() as i32,
             d.nanosecond() as i32, 0)
        }
        TzArg::Local => {
            let d = utc_dt.with_timezone(&chrono::Local);
            let off = d.offset().fix().local_minus_utc() / 60;
            (d.year(), d.month() as i32, d.day() as i32,
             d.hour() as i32, d.minute() as i32, d.second() as i32,
             d.nanosecond() as i32, off)
        }
        TzArg::Offset(off_min) => {
            let off_secs = off_min.saturating_mul(60);
            let fixed = chrono::FixedOffset::east_opt(off_secs)
                .ok_or("to_components: offset out of range")?;
            let d = utc_dt.with_timezone(&fixed);
            (d.year(), d.month() as i32, d.day() as i32,
             d.hour() as i32, d.minute() as i32, d.second() as i32,
             d.nanosecond() as i32, *off_min)
        }
        TzArg::Iana(name) => {
            let tz: chrono_tz::Tz = name.parse()
                .map_err(|e| format!("to_components: unknown timezone `{name}`: {e}"))?;
            let d = utc_dt.with_timezone(&tz);
            let off = d.offset().fix().local_minus_utc() / 60;
            (d.year(), d.month() as i32, d.day() as i32,
             d.hour() as i32, d.minute() as i32, d.second() as i32,
             d.nanosecond() as i32, off)
        }
    };
    let mut rec = indexmap::IndexMap::new();
    rec.insert("year".into(),    Value::Int(y as i64));
    rec.insert("month".into(),   Value::Int(m as i64));
    rec.insert("day".into(),     Value::Int(d as i64));
    rec.insert("hour".into(),    Value::Int(hh as i64));
    rec.insert("minute".into(),  Value::Int(mm as i64));
    rec.insert("second".into(),  Value::Int(ss as i64));
    rec.insert("nano".into(),    Value::Int(ns as i64));
    rec.insert("tz_offset_minutes".into(), Value::Int(off_min as i64));
    let _ = chrono::Utc.timestamp_opt(0, 0); // touch TimeZone to suppress unused-import lint paths
    Ok(Value::Record(rec))
}


fn instant_from_components(rec: &indexmap::IndexMap<String, Value>) -> Result<i64, String> {
    use chrono::TimeZone;
    fn get_int(rec: &indexmap::IndexMap<String, Value>, k: &str) -> Result<i64, String> {
        match rec.get(k) {
            Some(Value::Int(n)) => Ok(*n),
            other => Err(format!("from_components: missing or non-int field `{k}`: {other:?}")),
        }
    }
    let y = get_int(rec, "year")? as i32;
    let m = get_int(rec, "month")? as u32;
    let d = get_int(rec, "day")? as u32;
    let hh = get_int(rec, "hour")? as u32;
    let mm = get_int(rec, "minute")? as u32;
    let ss = get_int(rec, "second")? as u32;
    let ns = get_int(rec, "nano")? as u32;
    let off_min = get_int(rec, "tz_offset_minutes")? as i32;
    let off = chrono::FixedOffset::east_opt(off_min * 60)
        .ok_or("from_components: offset out of range")?;
    let dt = off
        .with_ymd_and_hms(y, m, d, hh, mm, ss)
        .single()
        .ok_or("from_components: invalid or ambiguous date/time")?;
    let dt = dt + chrono::Duration::nanoseconds(ns as i64);
    Ok(instant_from_chrono(dt))
}
