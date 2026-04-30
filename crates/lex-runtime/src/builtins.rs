//! Pure stdlib builtins — string, numeric, list, option, result, json
//! ops dispatched via the same `EffectHandler` interface as effects, but
//! without policy gates (they have no observable side effects).

use indexmap::IndexMap;
use lex_bytecode::Value;

/// Returns Some(...) if `(kind, op)` names a known pure builtin.
/// `None` means "not handled here; fall through to effect dispatch".
pub fn try_pure_builtin(kind: &str, op: &str, args: &[Value]) -> Option<Result<Value, String>> {
    if !is_pure_module(kind) { return None; }
    Some(dispatch(kind, op, args))
}

/// `kind` is one of the known pure module aliases — used by the policy
/// walk to skip pure builtins that programs reference via imports.
pub fn is_pure_module(kind: &str) -> bool {
    matches!(kind, "str" | "int" | "float" | "bool" | "list" | "map" | "set"
        | "option" | "result" | "tuple" | "json" | "bytes" | "flow")
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

        // -- int / float --
        ("int", "to_str") => Ok(Value::Str(expect_int(args.first())?.to_string())),
        ("int", "to_float") => Ok(Value::Float(expect_int(args.first())? as f64)),
        ("float", "to_int") => Ok(Value::Int(expect_float(args.first())? as i64)),
        ("float", "to_str") => Ok(Value::Str(expect_float(args.first())?.to_string())),

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

        _ => Err(format!("unknown pure builtin: {kind}.{op}")),
    }
}

fn first_arg(args: &[Value]) -> Result<&Value, String> {
    args.first().ok_or_else(|| "missing argument".into())
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

fn some(v: Value) -> Value { Value::Variant { name: "Some".into(), args: vec![v] } }
fn none() -> Value { Value::Variant { name: "None".into(), args: Vec::new() } }
fn ok_v(v: Value) -> Value { Value::Variant { name: "Ok".into(), args: vec![v] } }
fn err_v(v: Value) -> Value { Value::Variant { name: "Err".into(), args: vec![v] } }

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::from(*n),
        Value::Float(f) => J::from(*f),
        Value::Bool(b) => J::Bool(*b),
        Value::Str(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(b.iter().map(|b| format!("{:02x}", b)).collect()),
        Value::Unit => J::Null,
        Value::List(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Tuple(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Record(fields) => {
            let mut m = serde_json::Map::new();
            for (k, v) in fields { m.insert(k.clone(), value_to_json(v)); }
            J::Object(m)
        }
        Value::Variant { name, args } => {
            let mut m = serde_json::Map::new();
            m.insert("$variant".into(), J::String(name.clone()));
            m.insert("args".into(), J::Array(args.iter().map(value_to_json).collect()));
            J::Object(m)
        }
        Value::Closure { fn_id, .. } => J::String(format!("<closure fn_{fn_id}>")),
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Unit,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() { Value::Int(i) }
            else if let Some(f) = n.as_f64() { Value::Float(f) }
            else { Value::Unit }
        }
        J::String(s) => Value::Str(s.clone()),
        J::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        J::Object(map) => {
            if let (Some(J::String(name)), Some(J::Array(args))) =
                (map.get("$variant"), map.get("args"))
            {
                return Value::Variant {
                    name: name.clone(),
                    args: args.iter().map(json_to_value).collect(),
                };
            }
            let mut out = IndexMap::new();
            for (k, v) in map { out.insert(k.clone(), json_to_value(v)); }
            Value::Record(out)
        }
    }
}
