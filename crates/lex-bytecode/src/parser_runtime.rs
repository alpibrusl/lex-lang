//! Parser combinator interpreter (#221).
//!
//! Lives in `lex-bytecode` rather than `lex-runtime` because it needs
//! to invoke `Value::Closure` values from `Map` / `AndThen` nodes,
//! which requires VM-level access. The structural primitives are
//! constructed by `lex-runtime::builtins`; only the recursive
//! interpretation step (`parser.run`) is here.
//!
//! Calling convention:
//!   - The Vm intercepts `("parser", "run")` effect dispatch before
//!     invoking the handler and routes the args to `run_parser`,
//!     passing itself as the `ClosureCaller`.
//!   - `Map(p, f)` and `AndThen(p, f)` AST nodes carry a closure
//!     value `f`; the interpreter calls back via `caller.call_closure`
//!     to invoke it on the parsed result.

use crate::Value;

/// Trait the parser interpreter uses to invoke captured closures
/// during the recursive walk. The Vm implements it via
/// `Vm::invoke_closure_value`, but the interpreter is generic over
/// the implementation so `lex-bytecode` doesn't need to depend on
/// any higher-level runtime concepts.
pub trait ClosureCaller {
    fn call_closure(&mut self, closure: Value, args: Vec<Value>) -> Result<Value, String>;
}

/// Walk a parser AST. Returns `(value, end_pos)` on success or
/// `(failure_pos, message)` on failure. The interpreter is the same
/// shape as the original (in `lex-runtime::builtins`) plus the
/// `Map` and `AndThen` cases that need closure invocation.
pub fn run_parser(
    node: &Value,
    input: &str,
    pos: usize,
    caller: &mut dyn ClosureCaller,
) -> Result<(Value, usize), (usize, String)> {
    let rec = match node {
        Value::Record(r) => r,
        _ => return Err((pos, "parser: expected Parser node".into())),
    };
    let kind = match rec.get("kind") {
        Some(Value::Str(s)) => s.as_str(),
        _ => return Err((pos, "parser: malformed node (no kind)".into())),
    };
    let bytes = input.as_bytes();
    match kind {
        "Char" => {
            let want = match rec.get("ch") {
                Some(Value::Str(s)) => s,
                _ => return Err((pos, "char: missing ch".into())),
            };
            let want_bytes = want.as_bytes();
            if pos + want_bytes.len() > bytes.len() {
                return Err((pos, format!("expected {want:?}, got EOF")));
            }
            if &bytes[pos..pos + want_bytes.len()] == want_bytes {
                Ok((Value::Str(want.clone()), pos + want_bytes.len()))
            } else {
                Err((pos, format!("expected {want:?}")))
            }
        }
        "String" => {
            let want = match rec.get("s") {
                Some(Value::Str(s)) => s,
                _ => return Err((pos, "string: missing s".into())),
            };
            if input[pos..].starts_with(want.as_str()) {
                Ok((Value::Str(want.clone()), pos + want.len()))
            } else {
                Err((pos, format!("expected {want:?}")))
            }
        }
        "Digit" => {
            if let Some(&b) = bytes.get(pos) {
                if b.is_ascii_digit() {
                    return Ok((Value::Str((b as char).to_string()), pos + 1));
                }
            }
            Err((pos, "expected digit".into()))
        }
        "Alpha" => {
            if let Some(&b) = bytes.get(pos) {
                if b.is_ascii_alphabetic() {
                    return Ok((Value::Str((b as char).to_string()), pos + 1));
                }
            }
            Err((pos, "expected alpha".into()))
        }
        "Whitespace" => {
            if let Some(&b) = bytes.get(pos) {
                if b.is_ascii_whitespace() {
                    return Ok((Value::Str((b as char).to_string()), pos + 1));
                }
            }
            Err((pos, "expected whitespace".into()))
        }
        "Eof" => {
            if pos == bytes.len() {
                Ok((Value::Unit, pos))
            } else {
                Err((pos, "expected EOF".into()))
            }
        }
        "Seq" => {
            let a = rec.get("a").ok_or((pos, "seq: missing a".to_string()))?;
            let b = rec.get("b").ok_or((pos, "seq: missing b".to_string()))?;
            let (va, p1) = run_parser(a, input, pos, caller)?;
            let (vb, p2) = run_parser(b, input, p1, caller)?;
            Ok((Value::Tuple(vec![va, vb]), p2))
        }
        "Alt" => {
            let a = rec.get("a").ok_or((pos, "alt: missing a".to_string()))?;
            let b = rec.get("b").ok_or((pos, "alt: missing b".to_string()))?;
            match run_parser(a, input, pos, caller) {
                Ok(r) => Ok(r),
                Err(_) => run_parser(b, input, pos, caller),
            }
        }
        "Many" => {
            let p = rec.get("p").ok_or((pos, "many: missing p".to_string()))?;
            let mut cur = pos;
            let mut out = Vec::new();
            loop {
                match run_parser(p, input, cur, caller) {
                    Ok((v, np)) if np > cur => { out.push(v); cur = np; }
                    _ => break,
                }
            }
            Ok((Value::List(out), cur))
        }
        "Optional" => {
            let p = rec.get("p").ok_or((pos, "optional: missing p".to_string()))?;
            match run_parser(p, input, pos, caller) {
                Ok((v, np)) => Ok((
                    Value::Variant { name: "Some".into(), args: vec![v] },
                    np,
                )),
                Err(_) => Ok((
                    Value::Variant { name: "None".into(), args: vec![] },
                    pos,
                )),
            }
        }
        "Map" => {
            // Map(p, f): run p, then call f on the result. The new
            // value replaces the parsed one; the position advances by
            // whatever p consumed.
            let p = rec.get("p").ok_or((pos, "map: missing p".to_string()))?;
            let f = rec.get("f").cloned().ok_or((pos, "map: missing f".to_string()))?;
            let (v, np) = run_parser(p, input, pos, caller)?;
            let mapped = caller.call_closure(f, vec![v])
                .map_err(|e| (pos, format!("map: closure failed: {e}")))?;
            Ok((mapped, np))
        }
        "AndThen" => {
            // AndThen(p, f): run p; call f with the result, which
            // returns *another parser*; run that parser starting at
            // the position p left off. Monadic bind.
            let p = rec.get("p").ok_or((pos, "and_then: missing p".to_string()))?;
            let f = rec.get("f").cloned()
                .ok_or((pos, "and_then: missing f".to_string()))?;
            let (v, np) = run_parser(p, input, pos, caller)?;
            let next_parser = caller.call_closure(f, vec![v])
                .map_err(|e| (np, format!("and_then: closure failed: {e}")))?;
            run_parser(&next_parser, input, np, caller)
        }
        other => Err((pos, format!("unknown parser kind: {other:?}"))),
    }
}
