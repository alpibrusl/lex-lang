//! `lex issue` — typed issues: units of work with a declared, verifiable
//! acceptance (#949 phase 1).
//!
//!   lex issue create --title T [--body B] --shape S [shape flags]
//!                    [--base OP] [--dep ID]... [--project P] [--store DIR]
//!   lex issue list  [--project P] [--state S] [--store DIR]
//!   lex issue next  [--project P] [--limit N] [--store DIR]
//!   lex issue show <id> [--store DIR]
//!   lex issue verify <id> | --project P [--at OP] [--store DIR]
//!   lex issue propose <id> --shape S [shape flags] [--rationale R] [--by WHO]
//!   lex issue proposals <id>
//!   lex issue approve|reject <proposal> --by WHO [--notes N]
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
    all_issue_status, check_refinable, effective_acceptance, evaluate_static, is_verified, prepare_example_stages,
    proposal_status, record_issue_verdict, record_proposal_review, with_effective_acceptance,
    IssueEvaluation, IssueState, IssueStatus,
};
use lex_store::{Store, StoreError};
use lex_vcs::{Acceptance, AcceptanceProposal, ApiChangeKind, ApiEntry, Issue, IssueLog};

use ::acli::OutputFormat;

use crate::acli;
use crate::store_root::parse_store_flag;

pub fn cmd_issue(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _activate, _dry_run) = parse_store_flag(args);
    let sub = rest.first().map(String::as_str).unwrap_or("");
    let tail = if rest.is_empty() { &rest[..] } else { &rest[1..] };
    match sub {
        "create" => create(fmt, &root, tail),
        "list" => list(fmt, &root, tail),
        "next" => next(fmt, &root, tail),
        "show" => show(&root, tail),
        "verify" => verify(fmt, &root, tail),
        "propose" => propose(fmt, &root, tail),
        "proposals" => proposals(fmt, &root, tail),
        "approve" => review(fmt, &root, tail, true),
        "reject" => review(fmt, &root, tail, false),
        _ => bail!(
            "usage: lex issue <create|list|next|show|verify|propose|proposals|approve|reject> [--store DIR]\n\
             list: [--project P] [--state open|in_progress|verified|blocked]  every issue with its derived state\n\
             next: [--project P] [--limit N]  issues ready to start: not verified, every dependency verified\n\
             propose: <issue> --shape S [shape flags] [--rationale R] [--by WHO]  propose a typed acceptance for a free_form issue\n\
             proposals: <issue>  list proposals with their status (pending|approved|rejected)\n\
             approve|reject: <proposal> --by WHO [--notes N]  a human verdict on a proposal\n\
             verify: <id> | --project P [--at OP]  evaluate one issue — or every issue of a project, in dependency order — at a head (default: branch head)\n\
             create: --title T [--body B] --shape typed_delta|failing_example|metric_invariant|evidence|free_form\n\
             \x20       [--api name:sig[:kind]]... [--example E]... [--predicate P --window W]\n\
             \x20       [--subject S] [--invariant I]... [--base OP] [--dep ID]... [--project P]"
        ),
    }
}

/// The shape flags `create` and `propose` share.
#[derive(Default)]
struct AcceptanceFlags {
    shape: Option<String>,
    api: Vec<ApiEntry>,
    examples: Vec<String>,
    predicate: Option<String>,
    window: Option<String>,
    subject: Option<String>,
    invariants: Vec<String>,
}

impl AcceptanceFlags {
    /// Consume `flag` (and its value) if it is a shape flag. `Ok(false)`
    /// means "not mine" — the caller handles it.
    fn take(&mut self, flag: &str, val: &mut dyn FnMut(&str) -> Result<String>) -> Result<bool> {
        match flag {
            "--shape" => self.shape = Some(val("--shape")?),
            "--api" => self.api.push(parse_api_entry(&val("--api")?)?),
            "--example" => self.examples.push(val("--example")?),
            "--predicate" => self.predicate = Some(val("--predicate")?),
            "--window" => self.window = Some(val("--window")?),
            "--subject" => self.subject = Some(val("--subject")?),
            "--invariant" => self.invariants.push(val("--invariant")?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn build(self) -> Result<Acceptance> {
        let AcceptanceFlags { shape, api, mut examples, predicate, window, subject, invariants } = self;
        let shape = shape.ok_or_else(|| anyhow!("--shape is required"))?;
        Ok(match shape.as_str() {
            "typed_delta" => {
                if api.is_empty() {
                    bail!("typed_delta needs at least one --api name:signature");
                }
                Acceptance::TypedDelta { api, examples }
            }
            "failing_example" => {
                let example =
                    examples.pop().ok_or_else(|| anyhow!("failing_example needs --example"))?;
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
        })
    }
}

fn create(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut title: Option<String> = None;
    let mut body = String::new();
    let mut flags = AcceptanceFlags::default();
    let mut base: Option<String> = None;
    let mut deps: BTreeSet<String> = BTreeSet::new();
    let mut project: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value"))
        };
        if flags.take(a, &mut val)? {
            continue;
        }
        match a.as_str() {
            "--title" => title = Some(val("--title")?),
            "--body" => body = val("--body")?,
            "--base" => base = Some(val("--base")?),
            "--dep" => { deps.insert(val("--dep")?); }
            "--project" => project = Some(val("--project")?),
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let title = title.ok_or_else(|| anyhow!("--title is required"))?;
    let acceptance = flags.build()?;

    let issue = Issue::new(title, body, acceptance, base, deps, project);
    let log = IssueLog::open(root)?;
    log.put(&issue)?;
    let id = issue.issue_id.clone();
    let data = serde_json::json!({
        "issue_id": issue.issue_id,
        "shape": issue.acceptance.shape(),
        "title": issue.title,
        "project": issue.project,
    });
    // Text mode prints the bare id: harnesses (loom, lex-code) read one line.
    acli::emit_or_text("issue-create", data, fmt, move || println!("{id}"));
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
fn verify(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut id: Option<String> = None;
    let mut at: Option<String> = None;
    let mut project: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--at" => at = Some(it.next().cloned().ok_or_else(|| anyhow!("--at needs an op id"))?),
            "--project" => {
                project = Some(it.next().cloned().ok_or_else(|| anyhow!("--project needs a name"))?)
            }
            other if !other.starts_with("--") && id.is_none() => id = Some(other.to_string()),
            other => bail!(
                "unexpected arg `{other}` (usage: lex issue verify <id> | --project P [--at OP] [--store DIR])"
            ),
        }
    }
    let store = Store::open(root)?;
    let head = resolve_head(&store, at)?;
    if let Some(p) = project {
        if id.is_some() {
            bail!("pass an issue id or --project, not both");
        }
        return verify_project(fmt, root, &store, &p, &head);
    }
    let id = id.ok_or_else(|| {
        anyhow!("usage: lex issue verify <id> | --project P [--at OP] [--store DIR]")
    })?;
    let log = IssueLog::open(root)?;
    let issue = log.get(&id)?.ok_or_else(|| anyhow!("unknown issue `{id}`"))?;
    // A free-form issue refined by an approved proposal (#956) is judged
    // against that proposal; the id — and so the verdict's key — is the
    // issue's own.
    let issue = with_effective_acceptance(&store, &issue)?;
    let eval = evaluate_at(&store, &issue, &head)?;
    emit_verdict(fmt, &store, &issue, &id, &head, eval)
}

fn resolve_head(store: &Store, at: Option<String>) -> Result<String> {
    match at {
        Some(h) => Ok(h),
        None => {
            let branch = store.current_branch();
            store
                .get_branch(&branch)?
                .and_then(|b| b.head_op)
                .ok_or_else(|| anyhow!("branch `{branch}` has no head yet; pass --at OP"))
        }
    }
}

/// Evaluate one issue's acceptance at `head` — the dependency gate, then the
/// static half, then the examples — without recording or printing anything,
/// so `verify <id>` and `verify --project` cannot disagree about a verdict.
fn evaluate_at(store: &Store, issue: &Issue, head: &str) -> Result<IssueEvaluation> {
    // A blocked dependency gates everything else: `--dep` was recorded at
    // create time but nothing consulted it here, so an issue whose declared
    // dependency had never itself verified could still come back `verified`.
    // Recorded as `Inconclusive` (never a pass the gate didn't check, and
    // never a `Failed` either — the issue's own oracle was never evaluated).
    let mut unmet: Vec<String> = Vec::new();
    for dep in &issue.deps {
        if !is_verified(store, dep)? {
            unmet.push(dep.clone());
        }
    }
    if !unmet.is_empty() {
        return Ok(IssueEvaluation::not_evaluable(format!(
            "blocked on unverified dependenc{}: {}",
            if unmet.len() == 1 { "y" } else { "ies" },
            unmet.join(", ")
        )));
    }

    // 1. Everything that needs no execution (the typed delta's API check).
    let mut eval = evaluate_static(store, issue, head)?;

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
            match prepare_example_stages(store, head, &cases) {
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
    Ok(eval)
}

/// Dependency order over the issues of one project: an issue comes after
/// every project-internal dependency. A cycle (or a dep outside the project)
/// cannot deadlock it — whatever cannot be placed is appended in creation
/// order, and `evaluate_at` reports the unmet dependency as `inconclusive`.
fn dependency_order(mut issues: Vec<Issue>) -> Vec<Issue> {
    issues.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.issue_id.cmp(&b.issue_id)));
    let in_project: BTreeSet<String> = issues.iter().map(|i| i.issue_id.clone()).collect();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<Issue> = Vec::with_capacity(issues.len());
    let mut rest = issues;
    while !rest.is_empty() {
        let (ready, waiting): (Vec<Issue>, Vec<Issue>) = rest.into_iter().partition(|i| {
            i.deps.iter().all(|d| !in_project.contains(d) || placed.contains(d))
        });
        if ready.is_empty() {
            out.extend(waiting);
            break;
        }
        for i in &ready {
            placed.insert(i.issue_id.clone());
        }
        out.extend(ready);
        rest = waiting;
    }
    out
}

/// `lex issue verify --project P` — re-evaluate every issue of a project at
/// one head, in dependency order, recording each verdict. This is the
/// "no dependent regresses" half of the definition of done: closing one
/// issue must not have quietly broken an earlier one (an agent turn that
/// rewrites a file whole can drop another issue's verified function), and
/// nothing else re-checks the earlier ones. Exit 1 if any issue fails.
fn verify_project(
    fmt: &OutputFormat,
    root: &std::path::Path,
    store: &Store,
    project: &str,
    head: &str,
) -> Result<()> {
    let log = IssueLog::open(root)?;
    let mut issues: Vec<Issue> = Vec::new();
    for id in log.list_ids()? {
        if let Some(i) = log.get(&id)? {
            if i.project.as_deref() == Some(project) {
                issues.push(i);
            }
        }
    }
    if issues.is_empty() {
        bail!("no issues in project `{project}`");
    }
    let mut rows = Vec::new();
    let (mut verified, mut failed, mut inconclusive) = (0usize, 0usize, 0usize);
    for issue in dependency_order(issues) {
        let judged = with_effective_acceptance(store, &issue)?;
        let eval = evaluate_at(store, &judged, head)?;
        record_issue_verdict(store, &judged, head, &eval)?;
        let (verdict, detail) = match &eval {
            IssueEvaluation::Passed => {
                verified += 1;
                ("verified", String::new())
            }
            IssueEvaluation::NotEvaluable { reason } => {
                inconclusive += 1;
                ("inconclusive", reason.clone())
            }
            IssueEvaluation::Failed { detail } => {
                failed += 1;
                ("failed", detail.clone())
            }
        };
        rows.push((issue.issue_id.clone(), issue.title.clone(), verdict, detail));
    }
    let data = serde_json::json!({
        "project": project,
        "head_op": head,
        "verified": verified,
        "failed": failed,
        "inconclusive": inconclusive,
        "issues": rows.iter().map(|(id, title, verdict, detail)| serde_json::json!({
            "issue_id": id, "title": title, "verdict": verdict, "detail": detail,
        })).collect::<Vec<_>>(),
    });
    let text_rows = rows.clone();
    let total = rows.len();
    acli::emit_or_text("issue-verify-project", data, fmt, move || {
        for (id, title, verdict, detail) in &text_rows {
            let tail = if detail.is_empty() { String::new() } else { format!("  — {detail}") };
            println!("{verdict:<12}  {id}  {title}{tail}");
        }
        println!("{verified}/{total} verified, {failed} failed, {inconclusive} inconclusive");
    });
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Record `eval` as the issue's verdict at `head` and print it — the tail
/// both the blocked-on-a-dependency short-circuit and a fully-evaluated
/// verdict share, so the two paths can't drift in what they report or how
/// they exit.
fn emit_verdict(
    fmt: &OutputFormat,
    store: &Store,
    issue: &Issue,
    id: &str,
    head: &str,
    eval: IssueEvaluation,
) -> Result<()> {
    let attestation = record_issue_verdict(store, issue, head, &eval)?;
    let (verdict, detail) = match &eval {
        IssueEvaluation::Passed => ("verified", String::new()),
        IssueEvaluation::NotEvaluable { reason } => ("inconclusive", reason.clone()),
        IssueEvaluation::Failed { detail } => ("failed", detail.clone()),
    };
    let data = serde_json::json!({
        "issue_id": id,
        "verdict": verdict,
        "detail": detail,
        "head_op": head,
        "attestation_id": attestation,
    });
    let line = match &eval {
        IssueEvaluation::Passed => format!("verified: {id} at {head} (attestation {attestation})"),
        IssueEvaluation::NotEvaluable { reason } => {
            format!("inconclusive: {reason} (attestation {attestation})")
        }
        IssueEvaluation::Failed { detail } => {
            format!("failed: {detail} (attestation {attestation})")
        }
    };
    acli::emit_or_text("issue-verify", data, fmt, move || println!("{line}"));
    if matches!(eval, IssueEvaluation::Failed { .. }) {
        // Exit 1 so scripts can gate on the verdict; the JSON/text above
        // already carries the detail.
        std::process::exit(1);
    }
    Ok(())
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

/// The flags `list` and `next` share: `--project P`, and a value-taking
/// `--state S` / `--limit N` where the command uses them.
fn parse_board_flags(args: &[String], allow_state: bool, allow_limit: bool) -> Result<(Option<String>, Option<String>, Option<usize>)> {
    let (mut project, mut state, mut limit) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--project" => {
                project = Some(it.next().cloned().ok_or_else(|| anyhow!("--project needs a name"))?)
            }
            "--state" if allow_state => {
                let v = it.next().cloned().ok_or_else(|| anyhow!("--state needs a value"))?;
                if !matches!(v.as_str(), "open" | "in_progress" | "verified" | "blocked") {
                    bail!("unknown state `{v}` (open|in_progress|verified|blocked)");
                }
                state = Some(v);
            }
            "--limit" if allow_limit => {
                let v = it.next().ok_or_else(|| anyhow!("--limit needs a number"))?;
                limit = Some(v.parse().map_err(|_| anyhow!("--limit needs a number, got `{v}`"))?);
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    Ok((project, state, limit))
}

fn state_name(s: IssueState) -> &'static str {
    match s {
        IssueState::Open => "open",
        IssueState::InProgress => "in_progress",
        IssueState::Verified => "verified",
        IssueState::Blocked => "blocked",
    }
}

/// Every issue's derived state, in creation order — so the order a planner
/// filed a project's issues in is the order `list` and `next` report them.
fn board(root: &std::path::Path, project: &Option<String>) -> Result<Vec<IssueStatus>> {
    let store = Store::open(root)?;
    let mut rows = all_issue_status(&store)?;
    rows.retain(|r| project.as_ref().is_none_or(|p| r.issue.project.as_deref() == Some(p.as_str())));
    rows.sort_by(|a, b| {
        a.issue.created_at.cmp(&b.issue.created_at).then(a.issue.issue_id.cmp(&b.issue.issue_id))
    });
    Ok(rows)
}

fn counts(rows: &[IssueStatus]) -> serde_json::Value {
    let n = |s: IssueState| rows.iter().filter(|r| r.state == s).count();
    serde_json::json!({
        "open": n(IssueState::Open),
        "in_progress": n(IssueState::InProgress),
        "verified": n(IssueState::Verified),
        "blocked": n(IssueState::Blocked),
        "total": rows.len(),
    })
}

fn row_json(r: &IssueStatus) -> serde_json::Value {
    serde_json::json!({
        "issue_id": r.issue.issue_id,
        "title": r.issue.title,
        "shape": r.issue.acceptance.shape(),
        "state": state_name(r.state),
        "project": r.issue.project,
        "deps": r.issue.deps,
        "blocked_on": r.blocked_on,
    })
}

/// `lex issue list [--project P] [--state S]` — each issue with the state
/// derived from the log (open, in progress, verified, blocked).
fn list(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let (project, state, _) = parse_board_flags(args, true, false)?;
    let mut rows = board(root, &project)?;
    let summary = counts(&rows);
    if let Some(s) = &state {
        rows.retain(|r| state_name(r.state) == s);
    }
    let data = serde_json::json!({
        "project": project,
        "counts": summary,
        "issues": rows.iter().map(row_json).collect::<Vec<_>>(),
    });
    acli::emit_or_text("issue-list", data, fmt, move || {
        for r in &rows {
            println!(
                "{}  {:<16}  {:<11}  {}",
                r.issue.issue_id,
                r.issue.acceptance.shape(),
                state_name(r.state),
                r.issue.title
            );
        }
    });
    Ok(())
}

/// `lex issue next [--project P] [--limit N]` — the issues that can start
/// now: not verified, and every dependency verified. Text mode prints bare
/// ids, one per line (what a harness reads); JSON also says whether the
/// board is `done` (everything verified) so a driver can tell "finished"
/// from "nothing ready but work remains" (stuck).
fn next(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let (project, _, limit) = parse_board_flags(args, false, true)?;
    let rows = board(root, &project)?;
    let summary = counts(&rows);
    let done = !rows.is_empty() && rows.iter().all(|r| r.state == IssueState::Verified);
    let mut ready: Vec<&IssueStatus> = rows
        .iter()
        .filter(|r| matches!(r.state, IssueState::Open | IssueState::InProgress))
        .collect();
    if let Some(n) = limit {
        ready.truncate(n);
    }
    let data = serde_json::json!({
        "project": project,
        "done": done,
        "counts": summary,
        "ready": ready.iter().map(|r| row_json(r)).collect::<Vec<_>>(),
    });
    let ids: Vec<String> = ready.iter().map(|r| r.issue.issue_id.clone()).collect();
    acli::emit_or_text("issue-next", data, fmt, move || {
        for id in &ids {
            println!("{id}");
        }
    });
    Ok(())
}

fn show(root: &std::path::Path, args: &[String]) -> Result<()> {
    let id = args.first().ok_or_else(|| anyhow!("usage: lex issue show <id>"))?;
    let log = IssueLog::open(root)?;
    match log.get(id)? {
        Some(issue) => {
            // The issue as stored, plus — once a proposal is approved (#956)
            // — the acceptance the gate actually evaluates. Additive, so a
            // reader that only knows `acceptance` keeps working.
            let mut out = serde_json::to_value(&issue)?;
            if let Ok(store) = Store::open(root) {
                if let (acceptance, Some(pid)) = effective_acceptance(&store, &issue)? {
                    out["effective_acceptance"] = serde_json::to_value(&acceptance)?;
                    out["approved_proposal"] = serde_json::Value::String(pid);
                }
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        None => bail!("unknown issue `{id}`"),
    }
}

/// `lex issue propose <issue> --shape S [shape flags] [--rationale R] [--by WHO]`
/// — propose a typed acceptance for a free-form issue (#956). The agent
/// does the spec labor; nothing changes until a human approves.
fn propose(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let mut id: Option<String> = None;
    let mut flags = AcceptanceFlags::default();
    let mut rationale = String::new();
    let mut by = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| -> Result<String> {
            it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value"))
        };
        if flags.take(a, &mut val)? {
            continue;
        }
        match a.as_str() {
            "--rationale" => rationale = val("--rationale")?,
            "--by" => by = val("--by")?,
            other if !other.starts_with("--") && id.is_none() => id = Some(other.to_string()),
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let id = id.ok_or_else(|| anyhow!("usage: lex issue propose <issue> --shape S [shape flags]"))?;
    let acceptance = flags.build()?;
    if !acceptance.is_machine_evaluable() {
        bail!("a proposal must be a typed acceptance, not free_form");
    }
    let log = IssueLog::open(root)?;
    let issue = log.get(&id)?.ok_or_else(|| anyhow!("unknown issue `{id}`"))?;
    check_refinable(&issue)?;
    let p = AcceptanceProposal::new(id.clone(), acceptance, rationale, by);
    log.put_proposal(&p)?;
    let store = Store::open(root)?;
    let status = proposal_status(&store, &p.proposal_id)?;
    let pid = p.proposal_id.clone();
    let data = serde_json::json!({
        "proposal_id": p.proposal_id,
        "issue_id": id,
        "shape": p.acceptance.shape(),
        "status": status,
    });
    acli::emit_or_text("issue-propose", data, fmt, move || println!("{pid}"));
    Ok(())
}

/// `lex issue proposals <issue>` — every proposal with its status.
fn proposals(fmt: &OutputFormat, root: &std::path::Path, args: &[String]) -> Result<()> {
    let id = args.first().ok_or_else(|| anyhow!("usage: lex issue proposals <issue>"))?;
    let log = IssueLog::open(root)?;
    log.get(id)?.ok_or_else(|| anyhow!("unknown issue `{id}`"))?;
    let store = Store::open(root)?;
    let mut rows = Vec::new();
    for p in log.proposals_for(id)? {
        let status = proposal_status(&store, &p.proposal_id)?;
        rows.push((p, status));
    }
    let data = serde_json::json!({
        "issue_id": id,
        "proposals": rows.iter().map(|(p, st)| {
            let mut v = serde_json::to_value(p).unwrap_or_default();
            v["status"] = serde_json::to_value(st).unwrap_or_default();
            v
        }).collect::<Vec<_>>(),
    });
    let lines: Vec<String> = rows
        .iter()
        .map(|(p, st)| {
            let st = serde_json::to_value(st).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            format!("{}  {:<9} {:<16} {}", p.proposal_id, st, p.acceptance.shape(), p.proposed_by)
        })
        .collect();
    acli::emit_or_text("issue-proposals", data, fmt, move || {
        for l in lines {
            println!("{l}");
        }
    });
    Ok(())
}

/// `lex issue approve|reject <proposal> --by WHO [--notes N]` — the human
/// arbiter's verdict, recorded as a `Review` attestation on the proposal.
/// `--by` is required: an approval nobody signed is not an approval.
fn review(fmt: &OutputFormat, root: &std::path::Path, args: &[String], approve: bool) -> Result<()> {
    let (verb, gerund, past) =
        if approve { ("approve", "approving", "approved") } else { ("reject", "rejecting", "rejected") };
    let mut id: Option<String> = None;
    let mut by: Option<String> = None;
    let mut notes: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--by" => by = Some(it.next().cloned().ok_or_else(|| anyhow!("--by needs a value"))?),
            "--notes" => notes = Some(it.next().cloned().ok_or_else(|| anyhow!("--notes needs a value"))?),
            other if !other.starts_with("--") && id.is_none() => id = Some(other.to_string()),
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let id = id.ok_or_else(|| anyhow!("usage: lex issue {verb} <proposal> --by WHO [--notes N]"))?;
    let by = by
        .filter(|b| !b.trim().is_empty())
        .ok_or_else(|| anyhow!("--by is required: name who is {gerund} this proposal"))?;
    let log = IssueLog::open(root)?;
    let p = log.get_proposal(&id)?.ok_or_else(|| anyhow!("unknown proposal `{id}`"))?;
    let store = Store::open(root)?;
    let attestation = record_proposal_review(&store, &p, &by, approve, notes)?;
    let status = proposal_status(&store, &p.proposal_id)?;
    let data = serde_json::json!({
        "proposal_id": p.proposal_id,
        "issue_id": p.issue_id,
        "status": status,
        "reviewer": by,
        "attestation_id": attestation,
    });
    let line = format!("{past}: {} for issue {} (attestation {attestation})", p.proposal_id, p.issue_id);
    acli::emit_or_text(&format!("issue-{verb}"), data, fmt, move || println!("{line}"));
    Ok(())
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
