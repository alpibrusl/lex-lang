//! Run a descriptor through the full pipeline and compare.

use crate::descriptor::{Descriptor, PolicyJson};
use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Outcome of running a single descriptor.
#[derive(Debug)]
pub enum Outcome {
    Pass,
    /// `{descriptor_name, what_we_saw, what_we_expected}`.
    Fail(String),
}

/// Aggregate report.
#[derive(Debug, Default)]
pub struct Report {
    pub passed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl Report {
    pub fn ok(&self) -> bool { self.failed.is_empty() }
    pub fn total(&self) -> usize { self.passed.len() + self.failed.len() }
}

pub fn run_directory(dir: impl AsRef<Path>) -> std::io::Result<Report> {
    let mut report = Report::default();
    let dir = dir.as_ref();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json") {
            let bytes = std::fs::read(&p)?;
            let mut desc: Descriptor = match serde_json::from_slice(&bytes) {
                Ok(d) => d,
                Err(e) => {
                    report.failed.push((
                        p.display().to_string(),
                        format!("invalid descriptor: {e}"),
                    ));
                    continue;
                }
            };
            // Resolve `source_file` relative to the descriptor.
            if let (None, Some(rel)) = (&desc.source, &desc.source_file) {
                let path = p.parent().map(|d| d.join(rel)).unwrap_or_else(|| rel.clone());
                desc.source = Some(std::fs::read_to_string(&path)?);
            }
            let label = desc.name.clone();
            match run_descriptor(&desc) {
                Outcome::Pass => report.passed.push(label),
                Outcome::Fail(why) => report.failed.push((label, why)),
            }
        }
    }
    Ok(report)
}

pub fn run_descriptor(desc: &Descriptor) -> Outcome {
    let src = match &desc.source {
        Some(s) => s.clone(),
        None => return Outcome::Fail("descriptor missing `source` or `source_file`".into()),
    };

    // 1) Parse.
    let prog = match parse_source(&src) {
        Ok(p) => p,
        Err(e) => return match_status(desc, "syntax_error", Some(&format!("{e}"))),
    };

    // 2) Type-check.
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let kind = errs.first().map(error_kind).unwrap_or_else(|| "type_error".into());
        return match_status(desc, &kind, Some(&format!("{errs:?}")));
    }

    // 3) Compile + policy.
    let bc = compile_program(&stages);
    let policy = build_policy(&desc.policy);
    if let Err(violations) = check_policy(&bc, &policy) {
        let kind = violations.first().map(|v| v.kind.clone()).unwrap_or_else(|| "policy".into());
        return match_status(desc, &kind, Some(&format!("{violations:?}")));
    }

    // No execution requested: descriptor wants only typecheck/policy.
    let func = match &desc.func {
        Some(f) => f.clone(),
        None => return match_status(desc, "ok", None),
    };

    // 4) Execute.
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let args: Vec<Value> = desc.input.iter().map(json_to_value).collect();
    match vm.call(&func, args) {
        Ok(v) => {
            if !desc.expected_status.is_ok() {
                return Outcome::Fail(format!(
                    "expected error_kind={:?} but program ran successfully → {}",
                    desc.expected_status.error_kind(),
                    serde_json::to_string(&value_to_json(&v)).unwrap_or_default(),
                ));
            }
            if let Some(expected) = &desc.expected_output {
                let got = value_to_json(&v);
                if &got != expected {
                    return Outcome::Fail(format!(
                        "output mismatch: expected {} got {}",
                        serde_json::to_string(expected).unwrap_or_default(),
                        serde_json::to_string(&got).unwrap_or_default(),
                    ));
                }
            }
            Outcome::Pass
        }
        Err(e) => match_status(desc, "runtime_error", Some(&format!("{e}"))),
    }
}

fn match_status(desc: &Descriptor, observed_kind: &str, detail: Option<&str>) -> Outcome {
    if desc.expected_status.is_ok() {
        if observed_kind == "ok" { Outcome::Pass }
        else {
            Outcome::Fail(format!(
                "expected ok but saw {observed_kind}{}",
                detail.map(|d| format!(": {d}")).unwrap_or_default(),
            ))
        }
    } else if let Some(expected) = desc.expected_status.error_kind() {
        if observed_kind == expected { Outcome::Pass }
        else {
            Outcome::Fail(format!(
                "expected error_kind={expected} but saw {observed_kind}{}",
                detail.map(|d| format!(": {d}")).unwrap_or_default(),
            ))
        }
    } else {
        Outcome::Fail("descriptor expected_status is malformed".into())
    }
}

fn build_policy(p: &PolicyJson) -> Policy {
    Policy {
        allow_effects: p.allow_effects.iter().cloned().collect::<BTreeSet<_>>(),
        allow_fs_read: p.allow_fs_read.iter().map(PathBuf::from).collect(),
        allow_fs_write: p.allow_fs_write.iter().map(PathBuf::from).collect(),
        allow_net_host: Vec::new(),
        budget: p.budget,
    }
}

fn error_kind(e: &lex_types::TypeError) -> String {
    let v = serde_json::to_value(e).unwrap_or_default();
    v.get("kind").and_then(|k| k.as_str()).unwrap_or("type_error").to_string()
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

fn value_to_json(v: &Value) -> serde_json::Value { v.to_json() }
