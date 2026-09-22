//! Typed issues — units of work with a declared, verifiable acceptance (#949).
//!
//! A GitHub issue is free text and "done" is a human judgment, which is why an
//! issue can never have a 1-1 relation to its implementation: there is nothing
//! to check against. Here an issue is a **typed intent with a declared oracle**
//! — its [`Acceptance`] — and *done is a proof the gate verifies at HEAD*
//! (phase 2, #951), not a status someone sets. This is the always-valid-HEAD
//! invariant extended from code to work items.
//!
//! The record mirrors [`crate::Intent`]: content-addressed identity (so the
//! same logical issue dedups and travels idempotently), one canonical-JSON
//! file per issue in the store, and transfer over the same object-sync path
//! as stages, intents and locks. An op that realizes an issue carries its id
//! in the op's intent, so provenance links issue ↔ intent ↔ ops ↔ attestation.
//!
//! Five acceptance shapes (the oracle kinds — deliberately not one mold):
//! typed delta, failing example, metric/invariant, evidence, free-form. Shapes
//! 1–4 are machine-evaluable; shape 5 is the explicit, human-closed exception.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::canonical;

/// Content-addressed identity of an issue: lowercase-hex SHA-256 of the
/// canonical form of everything but `created_at` (timestamp drift must not
/// change what issue this is).
pub type IssueId = String;

/// How a declared public-API entry is expected to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiChangeKind {
    #[default]
    Added,
    Changed,
    Removed,
}

/// One entry of a typed delta: a public declaration and the signature it
/// should have after the work (the shape `api-diff` reports, so the evaluator
/// can compare like with like).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEntry {
    pub name: String,
    pub signature: String,
    #[serde(default)]
    pub kind: ApiChangeKind,
}

/// The declared oracle — what must hold for the issue to be done. Tagged by
/// `shape` in JSON (`{"shape": "typed_delta", ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Acceptance {
    /// Declares the public API that should exist after, plus behavioral
    /// examples (Lex `examples {}` source). Done when `api-diff(base, head)`
    /// realizes the delta and the examples pass. Features, new modules,
    /// analytics/finance *functions*.
    TypedDelta {
        api: Vec<ApiEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        examples: Vec<String>,
    },
    /// A behavioral example that fails at `base`; fixed = it passes. A bug
    /// report *is* a reproducible failing example.
    FailingExample { example: String },
    /// A typed predicate over the event backbone that must hold over a
    /// window (`p99 < 200ms for 7d`, `churn <= X`, `balances reconcile`).
    /// Monitoring, ops, growth, product *outcomes*, finance invariants.
    MetricInvariant { predicate: String, window: String },
    /// An attested evidence chain exists and its invariants hold (finance
    /// close, compliance, custody).
    Evidence {
        subject: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invariants: Vec<String>,
    },
    /// Human-closed. The explicit exception — kept small, never the default.
    FreeForm {},
}

impl Acceptance {
    /// The shape's stable name, as it appears in JSON.
    pub fn shape(&self) -> &'static str {
        match self {
            Acceptance::TypedDelta { .. } => "typed_delta",
            Acceptance::FailingExample { .. } => "failing_example",
            Acceptance::MetricInvariant { .. } => "metric_invariant",
            Acceptance::Evidence { .. } => "evidence",
            Acceptance::FreeForm {} => "free_form",
        }
    }

    /// Whether the gate can evaluate this shape mechanically. Only the
    /// free-form shape is not — a human closes it and the record says so.
    pub fn is_machine_evaluable(&self) -> bool {
        !matches!(self, Acceptance::FreeForm {})
    }
}

/// The persisted issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub issue_id: IssueId,
    pub title: String,
    /// Free text is welcome — it just isn't the acceptance.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub acceptance: Acceptance,
    /// The head the acceptance is declared against (`api-diff(base, head)`
    /// for a typed delta; where a failing example is observed to fail).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Blocking dependencies: this issue is *blocked* until each is verified.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub deps: BTreeSet<IssueId>,
    /// Optional project membership (a project is a subgraph with a goal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Wall-clock seconds since epoch at creation. Excluded from `issue_id`.
    pub created_at: u64,
}

impl Issue {
    /// Build an issue and compute its content-addressed id, stamping the
    /// current wall clock. Use [`Issue::with_timestamp`] to control the
    /// timestamp (tests).
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        acceptance: Acceptance,
        base: Option<String>,
        deps: BTreeSet<IssueId>,
        project: Option<String>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::with_timestamp(title, body, acceptance, base, deps, project, now)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamp(
        title: impl Into<String>,
        body: impl Into<String>,
        acceptance: Acceptance,
        base: Option<String>,
        deps: BTreeSet<IssueId>,
        project: Option<String>,
        created_at: u64,
    ) -> Self {
        let title = title.into();
        let body = body.into();
        let issue_id = compute_issue_id(
            &title,
            &body,
            &acceptance,
            base.as_deref(),
            &deps,
            project.as_deref(),
        );
        Self { issue_id, title, body, acceptance, base, deps, project, created_at }
    }

    /// The id this issue's content hashes to. Equal to `issue_id` for a
    /// well-formed record; a peer receiving issues over the wire compares the
    /// two so a mismatched id (tampered or miscomputed) is refused instead of
    /// being filed under a name its content doesn't own.
    pub fn computed_id(&self) -> IssueId {
        compute_issue_id(
            &self.title,
            &self.body,
            &self.acceptance,
            self.base.as_deref(),
            &self.deps,
            self.project.as_deref(),
        )
    }

    /// `issue_id` matches the content hash.
    pub fn id_is_consistent(&self) -> bool {
        self.issue_id == self.computed_id()
    }
}

fn compute_issue_id(
    title: &str,
    body: &str,
    acceptance: &Acceptance,
    base: Option<&str>,
    deps: &BTreeSet<IssueId>,
    project: Option<&str>,
) -> IssueId {
    let view = CanonicalIssueView { title, body, acceptance, base, deps, project };
    canonical::hash(&view)
}

/// Hashable shadow of [`Issue`] omitting `issue_id` (being computed) and
/// `created_at` (timestamp drift would break dedup).
#[derive(Serialize)]
struct CanonicalIssueView<'a> {
    title: &'a str,
    body: &'a str,
    acceptance: &'a Acceptance,
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<&'a str>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    deps: &'a BTreeSet<IssueId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
}

// ---- Persistence -------------------------------------------------

/// Persistent log of [`Issue`] records: one canonical-JSON file per issue
/// under `<root>/issues/`, atomic writes via tempfile + rename, idempotent on
/// re-puts. Mirrors [`crate::IntentLog`].
pub struct IssueLog {
    dir: PathBuf,
}

impl IssueLog {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("issues");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, id: &IssueId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Persist an issue. Idempotent on existing ids (content-addressed, so
    /// the bytes must match).
    pub fn put(&self, issue: &Issue) -> io::Result<()> {
        let path = self.path(&issue.issue_id);
        if path.exists() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(issue)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, id: &IssueId) -> io::Result<Option<Issue>> {
        let path = self.path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let issue: Issue = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(issue))
    }

    /// Every issue id in the log, sorted. Issues can exist before any op
    /// references them (open work), so sync moves the whole log rather than
    /// only the ids reachable from pushed ops.
    pub fn list_ids(&self) -> io::Result<Vec<IssueId>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

// ---- Tests --------------------------------------------------------

// ---- Agent-refined acceptance (#956) ------------------------------------
//
// An issue may start free-form; an agent then *proposes* a typed acceptance
// for it, and a human approves or rejects the proposal. The issue itself is
// never rewritten — its id is a hash of its content, acceptance included,
// so "setting" the acceptance would make it a different issue and orphan
// every intent and verdict that names it. A proposal is its own
// content-addressed object that points at the issue; the approval is a
// `Review` attestation keyed by the proposal id; the issue's *effective*
// acceptance is its latest approved proposal. Rejected proposals stay in
// the log, so the issue keeps its proposal history.

pub type ProposalId = String;

/// A proposed acceptance for an issue, awaiting a human verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceProposal {
    pub proposal_id: ProposalId,
    pub issue_id: IssueId,
    pub acceptance: Acceptance,
    /// Why this acceptance captures the issue — the agent's case to the
    /// human arbiter. Not part of the identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    /// Who proposed it (an agent/model name, or a person). Not part of the
    /// identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposed_by: String,
    pub created_at: u64,
}

impl AcceptanceProposal {
    pub fn new(
        issue_id: impl Into<IssueId>,
        acceptance: Acceptance,
        rationale: impl Into<String>,
        proposed_by: impl Into<String>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::with_timestamp(issue_id, acceptance, rationale, proposed_by, now)
    }

    pub fn with_timestamp(
        issue_id: impl Into<IssueId>,
        acceptance: Acceptance,
        rationale: impl Into<String>,
        proposed_by: impl Into<String>,
        created_at: u64,
    ) -> Self {
        let issue_id = issue_id.into();
        let proposal_id = compute_proposal_id(&issue_id, &acceptance);
        Self {
            proposal_id,
            issue_id,
            acceptance,
            rationale: rationale.into(),
            proposed_by: proposed_by.into(),
            created_at,
        }
    }

    pub fn id_is_consistent(&self) -> bool {
        self.proposal_id == compute_proposal_id(&self.issue_id, &self.acceptance)
    }
}

/// Identity = (issue, acceptance): proposing the same acceptance for the
/// same issue twice is the same proposal, whoever proposed it and why.
fn compute_proposal_id(issue_id: &str, acceptance: &Acceptance) -> ProposalId {
    #[derive(Serialize)]
    struct View<'a> {
        proposal_for: &'a str,
        acceptance: &'a Acceptance,
    }
    canonical::hash(&View { proposal_for: issue_id, acceptance })
}

impl IssueLog {
    fn proposals_dir(&self) -> io::Result<PathBuf> {
        let dir = self.dir.join("proposals");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Idempotent, like `put`.
    pub fn put_proposal(&self, p: &AcceptanceProposal) -> io::Result<()> {
        let path = self.proposals_dir()?.join(format!("{}.json", p.proposal_id));
        if path.exists() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(p)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get_proposal(&self, id: &ProposalId) -> io::Result<Option<AcceptanceProposal>> {
        let path = self.proposals_dir()?.join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Every proposal for `issue_id`, oldest first.
    pub fn proposals_for(&self, issue_id: &IssueId) -> io::Result<Vec<AcceptanceProposal>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.proposals_dir()?)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let p: AcceptanceProposal = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if &p.issue_id == issue_id {
                out.push(p);
            }
        }
        out.sort_by(|a, b| (a.created_at, &a.proposal_id).cmp(&(b.created_at, &b.proposal_id)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gcd_delta() -> Acceptance {
        Acceptance::TypedDelta {
            api: vec![ApiEntry {
                name: "gcd".into(),
                signature: "(Int, Int) -> Int".into(),
                kind: ApiChangeKind::Added,
            }],
            examples: vec!["gcd(12, 8) == 4".into()],
        }
    }

    #[test]
    fn same_content_hashes_equal_regardless_of_timestamp() {
        let a = Issue::with_timestamp("add gcd", "", gcd_delta(), None, BTreeSet::new(), None, 1);
        let b = Issue::with_timestamp("add gcd", "", gcd_delta(), None, BTreeSet::new(), None, 999);
        assert_eq!(a.issue_id, b.issue_id, "created_at must not affect identity");
    }

    #[test]
    fn different_acceptance_hashes_differ() {
        let a = Issue::with_timestamp("x", "", gcd_delta(), None, BTreeSet::new(), None, 1);
        let b = Issue::with_timestamp(
            "x", "", Acceptance::FailingExample { example: "gcd(12, 8) == 4".into() },
            None, BTreeSet::new(), None, 1,
        );
        assert_ne!(a.issue_id, b.issue_id);
    }

    #[test]
    fn shape_tag_round_trips_through_json() {
        let i = Issue::with_timestamp("x", "b", gcd_delta(), Some("op_1".into()), BTreeSet::new(), None, 1);
        let json = serde_json::to_string(&i).unwrap();
        assert!(json.contains("\"shape\":\"typed_delta\""), "{json}");
        let back: Issue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, i);
        let ff = Issue::with_timestamp("y", "", Acceptance::FreeForm {}, None, BTreeSet::new(), None, 1);
        let json = serde_json::to_string(&ff).unwrap();
        assert!(json.contains("\"shape\":\"free_form\""), "{json}");
        assert!(!ff.acceptance.is_machine_evaluable());
        assert!(i.acceptance.is_machine_evaluable());
    }

    #[test]
    fn log_put_get_list_and_idempotent_put() {
        let tmp = tempfile::tempdir().unwrap();
        let log = IssueLog::open(tmp.path()).unwrap();
        let i = Issue::with_timestamp("x", "", gcd_delta(), None, BTreeSet::new(), None, 1);
        log.put(&i).unwrap();
        log.put(&i).unwrap(); // idempotent
        assert_eq!(log.get(&i.issue_id).unwrap(), Some(i.clone()));
        assert_eq!(log.list_ids().unwrap(), vec![i.issue_id.clone()]);
        assert_eq!(log.get(&"missing".to_string()).unwrap(), None);
    }

    #[test]
    fn proposal_identity_is_issue_plus_acceptance() {
        let a = AcceptanceProposal::with_timestamp("iss", gcd_delta(), "why", "qwen", 1);
        let b = AcceptanceProposal::with_timestamp("iss", gcd_delta(), "other reason", "human", 9);
        assert_eq!(a.proposal_id, b.proposal_id, "rationale/proposer/time are not identity");
        let c = AcceptanceProposal::with_timestamp("other", gcd_delta(), "why", "qwen", 1);
        assert_ne!(a.proposal_id, c.proposal_id, "a proposal is for one issue");
        assert!(a.id_is_consistent());
    }

    #[test]
    fn proposals_round_trip_and_do_not_leak_into_issue_ids() {
        let dir = tempfile::tempdir().unwrap();
        let log = IssueLog::open(dir.path()).unwrap();
        let issue = Issue::with_timestamp("vague", "", Acceptance::FreeForm {}, None, BTreeSet::new(), None, 1);
        log.put(&issue).unwrap();
        let p = AcceptanceProposal::with_timestamp(issue.issue_id.clone(), gcd_delta(), "", "", 2);
        log.put_proposal(&p).unwrap();
        log.put_proposal(&p).unwrap();
        assert_eq!(log.get_proposal(&p.proposal_id).unwrap(), Some(p.clone()));
        assert_eq!(log.proposals_for(&issue.issue_id).unwrap(), vec![p]);
        assert!(log.proposals_for(&"nope".to_string()).unwrap().is_empty());
        // `list_ids` (which op push syncs) must still see only issues.
        assert_eq!(log.list_ids().unwrap(), vec![issue.issue_id]);
    }
}
