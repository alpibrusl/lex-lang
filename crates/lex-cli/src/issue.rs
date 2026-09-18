//! `lex issue` — typed issues: units of work with a declared, verifiable
//! acceptance (#949 phase 1).
//!
//!   lex issue create --title T [--body B] --shape S [shape flags]
//!                    [--base OP] [--dep ID]... [--project P] [--store DIR]
//!   lex issue list  [--store DIR]
//!   lex issue show <id> [--store DIR]
//!
//! Shapes and their flags:
//!   typed_delta      --api "name:signature[:added|changed|removed]"... [--example E]...
//!   failing_example  --example E
//!   metric_invariant --predicate P --window W
//!   evidence         --subject S [--invariant I]...
//!   free_form        (none — human-closed, the explicit exception)
//!
//! An issue is content-addressed and lives in `<store>/issues/`; it travels
//! with `lex op push`/`pull` like intents and locks.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use lex_vcs::{Acceptance, ApiChangeKind, ApiEntry, Issue, IssueLog};

use crate::store_root::parse_store_flag;

pub fn cmd_issue(args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let sub = rest.first().map(String::as_str).unwrap_or("");
    let tail = if rest.is_empty() { &rest[..] } else { &rest[1..] };
    match sub {
        "create" => create(&root, tail),
        "list" => list(&root),
        "show" => show(&root, tail),
        _ => bail!(
            "usage: lex issue <create|list|show> [--store DIR]\n\
             create: --title T [--body B] --shape typed_delta|failing_example|metric_invariant|evidence|free_form\n\
             \x20       [--api name:sig[:kind]]... [--example E]... [--predicate P --window W]\n\
             \x20       [--subject S] [--invariant I]... [--base OP] [--dep ID]... [--project P]"
        ),
    }
}

fn create(root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut title: Option<String> = None;
    let mut body = String::new();
    let mut shape: Option<String> = None;
    let mut api: Vec<ApiEntry> = Vec::new();
    let mut examples: Vec<String> = Vec::new();
    let mut predicate: Option<String> = None;
    let mut window: Option<String> = None;
    let mut subject: Option<String> = None;
    let mut invariants: Vec<String> = Vec::new();
    let mut base: Option<String> = None;
    let mut deps: BTreeSet<String> = BTreeSet::new();
    let mut project: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value"))
        };
        match a.as_str() {
            "--title" => title = Some(val("--title")?),
            "--body" => body = val("--body")?,
            "--shape" => shape = Some(val("--shape")?),
            "--api" => api.push(parse_api_entry(&val("--api")?)?),
            "--example" => examples.push(val("--example")?),
            "--predicate" => predicate = Some(val("--predicate")?),
            "--window" => window = Some(val("--window")?),
            "--subject" => subject = Some(val("--subject")?),
            "--invariant" => invariants.push(val("--invariant")?),
            "--base" => base = Some(val("--base")?),
            "--dep" => { deps.insert(val("--dep")?); }
            "--project" => project = Some(val("--project")?),
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let title = title.ok_or_else(|| anyhow!("--title is required"))?;
    let shape = shape.ok_or_else(|| anyhow!("--shape is required"))?;

    let acceptance = match shape.as_str() {
        "typed_delta" => {
            if api.is_empty() {
                bail!("typed_delta needs at least one --api name:signature");
            }
            Acceptance::TypedDelta { api, examples }
        }
        "failing_example" => {
            let example = examples.pop().ok_or_else(|| anyhow!("failing_example needs --example"))?;
            if !examples.is_empty() {
                bail!("failing_example takes exactly one --example");
            }
            Acceptance::FailingExample { example }
        }
        "metric_invariant" => Acceptance::MetricInvariant {
            predicate: predicate.ok_or_else(|| anyhow!("metric_invariant needs --predicate"))?,
            window: window.ok_or_else(|| anyhow!("metric_invariant needs --window"))?,
        },
        "evidence" => Acceptance::Evidence {
            subject: subject.ok_or_else(|| anyhow!("evidence needs --subject"))?,
            invariants,
        },
        "free_form" => Acceptance::FreeForm {},
        other => bail!(
            "unknown shape `{other}` (typed_delta|failing_example|metric_invariant|evidence|free_form)"
        ),
    };

    let issue = Issue::new(title, body, acceptance, base, deps, project);
    let log = IssueLog::open(root)?;
    log.put(&issue)?;
    println!("{}", issue.issue_id);
    Ok(())
}

/// `name:signature[:added|changed|removed]` — the signature may itself
/// contain `:` (`(a :: Int) -> Int`), so split the kind off the end only
/// when the last segment is a known kind.
fn parse_api_entry(s: &str) -> Result<ApiEntry> {
    let (name, rest) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("--api expects name:signature[:kind], got `{s}`"))?;
    let (signature, kind) = match rest.rsplit_once(':') {
        Some((sig, k)) if matches!(k, "added" | "changed" | "removed") => (
            sig,
            match k {
                "added" => ApiChangeKind::Added,
                "changed" => ApiChangeKind::Changed,
                _ => ApiChangeKind::Removed,
            },
        ),
        _ => (rest, ApiChangeKind::Added),
    };
    if name.is_empty() || signature.is_empty() {
        bail!("--api expects name:signature[:kind], got `{s}`");
    }
    Ok(ApiEntry { name: name.to_string(), signature: signature.to_string(), kind })
}

fn list(root: &std::path::Path) -> Result<()> {
    let log = IssueLog::open(root)?;
    for id in log.list_ids()? {
        if let Some(issue) = log.get(&id)? {
            println!("{}  {:<16}  {}", issue.issue_id, issue.acceptance.shape(), issue.title);
        }
    }
    Ok(())
}

fn show(root: &std::path::Path, args: &[String]) -> Result<()> {
    let id = args.first().ok_or_else(|| anyhow!("usage: lex issue show <id>"))?;
    let log = IssueLog::open(root)?;
    match log.get(id)? {
        Some(issue) => {
            println!("{}", serde_json::to_string_pretty(&issue)?);
            Ok(())
        }
        None => bail!("unknown issue `{id}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_entry_parses_signature_with_colons_and_optional_kind() {
        let e = parse_api_entry("gcd:(a :: Int, b :: Int) -> Int").unwrap();
        assert_eq!(e.name, "gcd");
        assert_eq!(e.signature, "(a :: Int, b :: Int) -> Int");
        assert_eq!(e.kind, ApiChangeKind::Added);
        let e = parse_api_entry("gcd:(Int, Int) -> Int:changed").unwrap();
        assert_eq!(e.kind, ApiChangeKind::Changed);
        assert_eq!(e.signature, "(Int, Int) -> Int");
        assert!(parse_api_entry("nocolon").is_err());
    }
}
