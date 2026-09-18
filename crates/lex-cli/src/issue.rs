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
use lex_store::issues::{
    evaluate_static, prepare_example_stages, record_issue_verdict, IssueEvaluation,
};
use lex_store::{Store, StoreError};
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
        "verify" => verify(&root, tail),
        _ => bail!(
            "usage: lex issue <create|list|show|verify> [--store DIR]\n\
             verify: <id> [--at OP]  evaluate the issue's acceptance at a head (default: branch head)\n\
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

/// `lex issue verify <id> [--at OP]` — evaluate the issue's declared
/// acceptance at a head and record the verdict as an `IssueVerified`
/// attestation. Done is a proof, not a status: the static half
/// (`evaluate_static`: the API delta) runs in lex-store; the examples half
/// runs here, because running code needs lex-runtime, which lex-store can't
/// depend on. Exit 1 when the oracle fails, so a script can gate on it.
fn verify(root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut id: Option<String> = None;
    let mut at: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--at" => at = Some(it.next().cloned().ok_or_else(|| anyhow!("--at needs an op id"))?),
            other if !other.starts_with("--") && id.is_none() => id = Some(other.to_string()),
            other => bail!("unexpected arg `{other}` (usage: lex issue verify <id> [--at OP] [--store DIR])"),
        }
    }
    let id = id.ok_or_else(|| anyhow!("usage: lex issue verify <id> [--at OP] [--store DIR]"))?;
    let log = IssueLog::open(root)?;
    let issue = log.get(&id)?.ok_or_else(|| anyhow!("unknown issue `{id}`"))?;
    let store = Store::open(root)?;
    let head = match at {
        Some(h) => h,
        None => {
            let branch = store.current_branch();
            store
                .get_branch(&branch)?
                .and_then(|b| b.head_op)
                .ok_or_else(|| anyhow!("branch `{branch}` has no head yet; pass --at OP"))?
        }
    };

    // 1. Everything that needs no execution (the typed delta's API check).
    let mut eval = evaluate_static(&store, &issue, &head)?;

    // 2. The examples, only if the static half held and the shape carries any.
    if eval.is_passed() {
        let cases_src: Vec<&str> = match &issue.acceptance {
            Acceptance::TypedDelta { examples, .. } => examples.iter().map(String::as_str).collect(),
            Acceptance::FailingExample { example } => vec![example.as_str()],
            _ => Vec::new(),
        };
        if !cases_src.is_empty() {
            let mut cases = Vec::with_capacity(cases_src.len());
            for c in &cases_src {
                cases.push(parse_example_case(c)?);
            }
            match prepare_example_stages(&store, &head, &cases) {
                Ok(stages) => {
                    let errs = lex_runtime::evaluate_examples(&stages);
                    if !errs.is_empty() {
                        let detail = errs
                            .iter()
                            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| "example mismatch".into()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        eval = IssueEvaluation::failed(detail);
                    }
                }
                Err(StoreError::IssueTarget(name)) => {
                    eval = IssueEvaluation::failed(format!(
                        "example targets `{name}`, which is not declared at this head"
                    ));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    let attestation = record_issue_verdict(&store, &issue, &head, &eval)?;
    match &eval {
        IssueEvaluation::Passed => {
            println!("verified: {id} at {head} (attestation {attestation})");
            Ok(())
        }
        IssueEvaluation::NotEvaluable { reason } => {
            println!("inconclusive: {reason} (attestation {attestation})");
            Ok(())
        }
        IssueEvaluation::Failed { detail } => {
            println!("failed: {detail} (attestation {attestation})");
            std::process::exit(1);
        }
    }
}

/// `name(args) => expected` → `(name, Example)`. Parsed under a stub fn
/// named after the callee: the parser keeps a case's args and expected
/// value (not its callee), and never type-checks the stub, so `-> Int { 0 }`
/// is fine for any signature.
fn parse_example_case(case: &str) -> Result<(String, lex_ast::Example)> {
    let name = case
        .split('(')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("example `{case}` must be `name(args) => expected`"))?
        .to_string();
    let src = format!("fn {name}() -> Int examples {{ {case} }} {{ 0 }}\n");
    let prog = lex_syntax::parse_source(&src)
        .map_err(|e| anyhow!("parsing example `{case}`: {e:?}"))?;
    let example = lex_ast::canonicalize_program(&prog)
        .into_iter()
        .find_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => fd.examples.into_iter().next(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("example `{case}` parsed to no case"))?;
    Ok((name, example))
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
