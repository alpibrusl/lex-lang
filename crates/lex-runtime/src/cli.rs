//! `std.cli` — argparse-equivalent for end-user Lex programs
//! (Rubric port follow-up).
//!
//! The Rubric CLI has six subcommands with a mixed bag of positional
//! / option / flag arguments, JSON-envelope output, and ACLI
//! introspection. Hand-rolling each subcommand's parser is days of
//! clipped-wing work; this module provides the equivalent Lex builder
//! surface.
//!
//! The wire format for spec values is JSON (opaque to user code,
//! constructed via `cli.flag` / `cli.option` / `cli.positional` /
//! `cli.spec`). Internal records carry a `kind` discriminator so
//! `parse` can identify what each entry is without relying on field
//! shape.

use serde_json::{json, Value};

// ---- Spec construction --------------------------------------------

pub fn flag_spec(name: &str, short: Option<&str>, help: &str) -> Value {
    json!({
        "kind": "flag",
        "name": name,
        "short": short,
        "help": help,
    })
}

pub fn option_spec(name: &str, short: Option<&str>, help: &str, default: Option<&str>) -> Value {
    json!({
        "kind": "option",
        "name": name,
        "short": short,
        "help": help,
        "default": default,
    })
}

pub fn positional_spec(name: &str, help: &str, required: bool) -> Value {
    json!({
        "kind": "positional",
        "name": name,
        "help": help,
        "required": required,
    })
}

pub fn build_spec(name: &str, help: &str, args: Vec<Value>, subcommands: Vec<Value>) -> Value {
    json!({
        "kind": "spec",
        "name": name,
        "help": help,
        "args": args,
        "subcommands": subcommands,
    })
}

// ---- Parsing ------------------------------------------------------

/// Parse `argv` against `spec`. Returns a `CliParsed`-shaped JSON
/// value on success, an error message on failure.
///
/// `CliParsed` shape:
/// ```json
/// {
///   "command":     ["rubric", "scan"],   // path of subcommand names
///   "flags":       { "verbose": true },
///   "options":     { "output": "report.json" },
///   "positionals": { "path": "./src" },
///   "remaining":   []                    // args after `--` separator
/// }
/// ```
pub fn parse(spec: &Value, argv: &[String]) -> Result<Value, String> {
    let mut state = ParseState::default();
    parse_into(spec, argv, 0, &mut state)?;
    Ok(json!({
        "command": state.command,
        "flags": state.flags,
        "options": state.options,
        "positionals": state.positionals,
        "remaining": state.remaining,
    }))
}

#[derive(Default)]
struct ParseState {
    command: Vec<String>,
    flags: serde_json::Map<String, Value>,
    options: serde_json::Map<String, Value>,
    positionals: serde_json::Map<String, Value>,
    remaining: Vec<String>,
}

fn parse_into(spec: &Value, argv: &[String], start: usize, state: &mut ParseState) -> Result<(), String> {
    let name = spec_name(spec);
    state.command.push(name.to_string());

    // Index this spec's args by long/short for cheap lookup.
    let args = spec_args(spec);
    let mut by_long: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
    let mut by_short: std::collections::HashMap<&str, &Value> = std::collections::HashMap::new();
    let mut positionals: Vec<&Value> = Vec::new();
    for a in args {
        let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "flag" | "option" => {
                if let Some(n) = a.get("name").and_then(|v| v.as_str()) {
                    by_long.insert(n, a);
                }
                if let Some(s) = a.get("short").and_then(|v| v.as_str()) {
                    by_short.insert(s, a);
                }
            }
            "positional" => positionals.push(a),
            _ => {}
        }
    }

    // Apply defaults for any options that have one — overwritten if
    // the option is later seen on the command line.
    for a in args {
        if a.get("kind").and_then(|v| v.as_str()) == Some("option") {
            if let (Some(n), Some(d)) = (
                a.get("name").and_then(|v| v.as_str()),
                a.get("default").and_then(|v| v.as_str()),
            ) {
                state.options.insert(n.to_string(), Value::String(d.to_string()));
            }
        }
    }
    // All flags default to `false` so consumers don't have to handle
    // the missing-key case.
    for a in args {
        if a.get("kind").and_then(|v| v.as_str()) == Some("flag") {
            if let Some(n) = a.get("name").and_then(|v| v.as_str()) {
                state.flags.insert(n.to_string(), Value::Bool(false));
            }
        }
    }

    let subcommands = spec_subcommands(spec);
    let sub_by_name: std::collections::HashMap<&str, &Value> = subcommands.iter()
        .filter_map(|s| spec_name_opt(s).map(|n| (n, s)))
        .collect();

    let mut i = start;
    let mut positional_idx = 0usize;
    while i < argv.len() {
        let tok = &argv[i];

        // `--` ends flag/option parsing; everything after is remainder.
        if tok == "--" {
            state.remaining.extend(argv[i + 1..].iter().cloned());
            return Ok(());
        }

        // Long flag / option: --name or --name=value.
        if let Some(rest) = tok.strip_prefix("--") {
            let (lname, inline_val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            let entry = by_long.get(lname).ok_or_else(|| format!(
                "unknown flag `--{lname}` for `{name}`"))?;
            apply_flag_or_option(entry, inline_val, &mut i, argv, state)?;
            i += 1;
            continue;
        }

        // Short flag: `-x` (single char) or `-x=value`.
        if let Some(rest) = tok.strip_prefix('-') {
            // Reject negative-number-as-positional collision: a token
            // like "-5" with no matching short flag is treated as a
            // positional value rather than an unknown flag.
            let (sname, inline_val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            if let Some(entry) = by_short.get(sname) {
                apply_flag_or_option(entry, inline_val, &mut i, argv, state)?;
                i += 1;
                continue;
            }
            // Fall through: treat as positional.
        }

        // Subcommand match (only at the first non-flag positional
        // *before* any explicit positional has been consumed).
        if positional_idx == 0 && !sub_by_name.is_empty() {
            if let Some(sub) = sub_by_name.get(tok.as_str()) {
                return parse_into(sub, argv, i + 1, state);
            }
        }

        // Positional.
        if let Some(p) = positionals.get(positional_idx) {
            let pname = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
            state.positionals.insert(pname.to_string(), Value::String(tok.clone()));
            positional_idx += 1;
        } else {
            return Err(format!(
                "unexpected positional argument `{tok}` for `{name}`"));
        }
        i += 1;
    }

    // Validate required positionals.
    for (idx, p) in positionals.iter().enumerate() {
        if idx >= positional_idx
            && p.get("required").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            let pname = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            return Err(format!(
                "missing required positional `{pname}` for `{name}`"));
        }
    }

    Ok(())
}

fn apply_flag_or_option(
    entry: &Value,
    inline_val: Option<String>,
    i: &mut usize,
    argv: &[String],
    state: &mut ParseState,
) -> Result<(), String> {
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    match kind {
        "flag" => {
            if let Some(v) = inline_val {
                return Err(format!(
                    "flag `--{name}` does not take a value (got `={v}`)"));
            }
            state.flags.insert(name.to_string(), Value::Bool(true));
        }
        "option" => {
            let val = match inline_val {
                Some(v) => v,
                None => {
                    let next = argv.get(*i + 1).ok_or_else(|| format!(
                        "option `--{name}` requires a value"))?;
                    *i += 1;
                    next.clone()
                }
            };
            state.options.insert(name.to_string(), Value::String(val));
        }
        _ => return Err(format!("internal: unexpected entry kind `{kind}`")),
    }
    Ok(())
}

// ---- Spec helpers -------------------------------------------------

fn spec_name(spec: &Value) -> &str {
    spec.get("name").and_then(|v| v.as_str()).unwrap_or("")
}

fn spec_name_opt(spec: &Value) -> Option<&str> {
    spec.get("name").and_then(|v| v.as_str())
}

fn spec_args(spec: &Value) -> &[Value] {
    spec.get("args").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

fn spec_subcommands(spec: &Value) -> &[Value] {
    spec.get("subcommands").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}

// ---- ACLI envelope + introspection + help -------------------------

/// `{ "ok": true|false, "command": "<name>", "data": <data> }`. The
/// shape mirrors `acli`'s output envelope so user programs can match
/// `lex`'s own `--output json` shape without each command rolling
/// their own.
pub fn envelope(ok: bool, command: &str, data: Value) -> Value {
    json!({
        "ok": ok,
        "command": command,
        "data": data,
    })
}

/// Machine-readable description of a spec — recurses through
/// subcommands. Useful for tools that want to discover a CLI's
/// surface without invoking `--help`.
pub fn describe(spec: &Value) -> Value {
    json!({
        "name": spec_name(spec),
        "help": spec.get("help").cloned().unwrap_or(Value::String(String::new())),
        "args": spec_args(spec).to_vec(),
        "subcommands": spec_subcommands(spec).iter().map(describe).collect::<Vec<_>>(),
    })
}

/// Human-readable help text. Layout matches `argparse`/`clap` for
/// familiarity. Subcommands are listed below the args.
pub fn help_text(spec: &Value) -> String {
    let mut out = String::new();
    out.push_str(spec_name(spec));
    if let Some(h) = spec.get("help").and_then(|v| v.as_str()) {
        if !h.is_empty() {
            out.push_str(" — ");
            out.push_str(h);
        }
    }
    out.push('\n');

    let args = spec_args(spec);
    let positionals: Vec<&Value> = args.iter()
        .filter(|a| a.get("kind").and_then(|v| v.as_str()) == Some("positional"))
        .collect();
    let flags: Vec<&Value> = args.iter()
        .filter(|a| matches!(a.get("kind").and_then(|v| v.as_str()), Some("flag") | Some("option")))
        .collect();

    if !positionals.is_empty() {
        out.push_str("\nUSAGE:\n  ");
        out.push_str(spec_name(spec));
        for p in &positionals {
            let n = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let required = p.get("required").and_then(|v| v.as_bool()).unwrap_or(false);
            if required {
                out.push_str(&format!(" <{n}>"));
            } else {
                out.push_str(&format!(" [{n}]"));
            }
        }
        out.push('\n');
    }

    if !flags.is_empty() {
        out.push_str("\nFLAGS:\n");
        for f in flags {
            let n = f.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let s = f.get("short").and_then(|v| v.as_str()).unwrap_or("");
            let h = f.get("help").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = if s.is_empty() {
                format!("      --{n}")
            } else {
                format!("  -{s}, --{n}")
            };
            out.push_str(&format!("{prefix:<24}  {h}\n"));
        }
    }

    let subs = spec_subcommands(spec);
    if !subs.is_empty() {
        out.push_str("\nSUBCOMMANDS:\n");
        for sub in subs {
            let n = spec_name(sub);
            let h = sub.get("help").and_then(|v| v.as_str()).unwrap_or("");
            out.push_str(&format!("  {n:<16}  {h}\n"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_simple() -> Value {
        build_spec(
            "rubric", "Rubric CLI",
            vec![
                flag_spec("verbose", Some("v"), "show debug output"),
                option_spec("output", Some("o"), "write report", None),
                positional_spec("path", "directory to scan", true),
            ],
            vec![],
        )
    }

    #[test]
    fn parse_simple_positional_and_flag() {
        let s = spec_simple();
        let parsed = parse(&s, &["./src".into(), "--verbose".into()]).unwrap();
        assert_eq!(parsed["positionals"]["path"], "./src");
        assert_eq!(parsed["flags"]["verbose"], true);
    }

    #[test]
    fn parse_short_flag() {
        let s = spec_simple();
        let parsed = parse(&s, &["./src".into(), "-v".into()]).unwrap();
        assert_eq!(parsed["flags"]["verbose"], true);
    }

    #[test]
    fn parse_option_with_separate_value() {
        let s = spec_simple();
        let parsed = parse(&s, &[
            "--output".into(), "report.json".into(), "./src".into(),
        ]).unwrap();
        assert_eq!(parsed["options"]["output"], "report.json");
        assert_eq!(parsed["positionals"]["path"], "./src");
    }

    #[test]
    fn parse_option_with_inline_equals() {
        let s = spec_simple();
        let parsed = parse(&s, &["--output=report.json".into(), "./src".into()]).unwrap();
        assert_eq!(parsed["options"]["output"], "report.json");
    }

    #[test]
    fn parse_default_option_value_is_present() {
        let s = build_spec("x", "", vec![
            option_spec("level", None, "verbosity", Some("info")),
        ], vec![]);
        let parsed = parse(&s, &[]).unwrap();
        assert_eq!(parsed["options"]["level"], "info");
    }

    #[test]
    fn parse_missing_required_positional_errors() {
        let s = spec_simple();
        let err = parse(&s, &[]).unwrap_err();
        assert!(err.contains("missing required") && err.contains("path"),
            "expected missing-positional error, got: {err}");
    }

    #[test]
    fn parse_unknown_flag_errors() {
        let s = spec_simple();
        let err = parse(&s, &["./src".into(), "--bogus".into()]).unwrap_err();
        assert!(err.contains("unknown") && err.contains("--bogus"),
            "expected unknown-flag error, got: {err}");
    }

    #[test]
    fn parse_flag_with_inline_value_errors() {
        let s = spec_simple();
        let err = parse(&s, &["--verbose=yes".into(), "./src".into()]).unwrap_err();
        assert!(err.contains("does not take a value"),
            "expected flag-no-value error, got: {err}");
    }

    #[test]
    fn parse_double_dash_collects_remaining() {
        let s = spec_simple();
        let parsed = parse(&s, &[
            "./src".into(), "--".into(),
            "--would-be-flag".into(), "extra".into(),
        ]).unwrap();
        assert_eq!(
            parsed["remaining"].as_array().unwrap(),
            &[Value::String("--would-be-flag".into()), Value::String("extra".into())],
        );
    }

    #[test]
    fn parse_subcommand_descends() {
        let s = build_spec(
            "rubric", "",
            vec![flag_spec("verbose", Some("v"), "")],
            vec![
                build_spec("scan", "scan a directory",
                    vec![positional_spec("path", "", true)],
                    vec![]),
                build_spec("init", "initialise", vec![], vec![]),
            ],
        );
        let parsed = parse(&s, &["scan".into(), "./src".into()]).unwrap();
        assert_eq!(parsed["command"], json!(["rubric", "scan"]));
        assert_eq!(parsed["positionals"]["path"], "./src");
    }

    #[test]
    fn unknown_flag_in_subcommand_errors() {
        // Subcommands have their own flag namespace. A flag declared
        // on the parent doesn't propagate to children — they must be
        // re-declared on the subcommand if needed.
        let s = build_spec(
            "rubric", "",
            vec![flag_spec("verbose", Some("v"), "")],
            vec![build_spec("scan", "", vec![], vec![])],
        );
        let err = parse(&s, &["scan".into(), "-v".into()]).unwrap_err();
        assert!(err.contains("unknown") || err.contains("unexpected"),
            "subcommand should reject parent's flag; got: {err}");
    }

    #[test]
    fn envelope_shape_is_acli_compatible() {
        let env = envelope(true, "rubric", json!({"hits": 3}));
        assert_eq!(env["ok"], true);
        assert_eq!(env["command"], "rubric");
        assert_eq!(env["data"]["hits"], 3);
    }

    #[test]
    fn describe_recurses_into_subcommands() {
        let s = build_spec(
            "rubric", "outer",
            vec![flag_spec("verbose", Some("v"), "")],
            vec![build_spec("scan", "scan dir", vec![], vec![])],
        );
        let d = describe(&s);
        assert_eq!(d["name"], "rubric");
        assert_eq!(d["help"], "outer");
        let subs = d["subcommands"].as_array().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0]["name"], "scan");
        assert_eq!(subs[0]["help"], "scan dir");
    }

    #[test]
    fn help_text_lists_args_and_subs() {
        let s = build_spec(
            "rubric", "Rubric CLI",
            vec![
                flag_spec("verbose", Some("v"), "noisy"),
                option_spec("output", Some("o"), "write to FILE", None),
                positional_spec("path", "directory", true),
            ],
            vec![build_spec("scan", "scan a directory", vec![], vec![])],
        );
        let h = help_text(&s);
        assert!(h.contains("rubric"));
        assert!(h.contains("Rubric CLI"));
        assert!(h.contains("--verbose"));
        assert!(h.contains("-v"));
        assert!(h.contains("--output"));
        assert!(h.contains("<path>"));
        assert!(h.contains("scan"));
        assert!(h.contains("scan a directory"));
    }
}
