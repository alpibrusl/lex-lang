//! Capability/policy layer per spec §7.4.
//!
//! Operators specify what effects are allowed before any execution starts.
//! The runtime walks the program's declared effects and aborts with a
//! structured violation if the program would exceed the policy. During
//! execution, individual effect calls are also gated through the same
//! policy so that scoped effects (fs paths, budget consumption) are caught
//! at call time.

use indexmap::IndexMap;
use lex_bytecode::program::{DeclaredEffect, EffectArg, Program};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Policy a program is run under. Empty allowlist = pure-only execution.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub allow_effects: BTreeSet<String>,
    pub allow_fs_read: Vec<PathBuf>,
    pub allow_fs_write: Vec<PathBuf>,
    /// Per-host scope on the [net] effect. Empty = any host (when
    /// [net] is in `allow_effects`); non-empty = only requests to
    /// these hosts succeed. Hosts compare against the URL's host
    /// substring (port-agnostic). Lets a tool be granted [net] but
    /// scoped to e.g. `api.openai.com` only — without this, [net]
    /// is a blank check to exfiltrate anywhere.
    pub allow_net_host: Vec<String>,
    /// Per-binary scope on the [proc] effect. Empty = ANY binary
    /// allowed once [proc] is granted (treat as a global escape
    /// hatch; only acceptable for trusted code). Non-empty =
    /// `proc.spawn(cmd, args)` must match `cmd` against the
    /// basename portion of one of these entries. Per-arg validation
    /// is the *caller's* responsibility — see SECURITY.md's
    /// "argument injection" note.
    pub allow_proc: Vec<String>,
    pub budget: Option<u64>,
}

impl Policy {
    pub fn pure() -> Self { Self::default() }

    pub fn permissive() -> Self {
        let mut s = BTreeSet::new();
        for k in [
            "io", "net", "time", "rand", "llm", "proc", "panic",
            "fs_read", "fs_write", "budget",
            // #184: agent-runtime effects.
            "llm_local", "llm_cloud", "a2a", "mcp",
            // #216: env-var access. Per-var scoping (`[env(NAME)]`)
            // arrives with the per-capability effect parameterization
            // work (#207); the flat `[env]` is the v1 surface.
            "env",
        ] {
            s.insert(k.to_string());
        }
        Self {
            allow_effects: s,
            allow_fs_read: Vec::new(),
            allow_fs_write: Vec::new(),
            allow_net_host: Vec::new(),
            allow_proc: Vec::new(),
            budget: None,
        }
    }
}

/// Structured policy violation, formatted to match spec §6.7's JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("policy violation: {kind} {detail}")]
pub struct PolicyViolation {
    pub kind: String,
    pub detail: String,
    /// Effect kind that was disallowed, or `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    /// Path that fell outside the allowlist, or `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// NodeId or function name; precise location of the offense.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

impl PolicyViolation {
    pub fn effect_not_allowed(effect: &str, at: impl Into<String>) -> Self {
        Self {
            kind: "effect_not_allowed".into(),
            detail: format!("effect `{effect}` not in --allow-effects"),
            effect: Some(effect.into()),
            path: None,
            at: Some(at.into()),
        }
    }
    pub fn fs_path_not_allowed(effect: &str, path: &str, at: impl Into<String>) -> Self {
        Self {
            kind: "fs_path_not_allowed".into(),
            detail: format!("path `{path}` outside --allow-{effect}"),
            effect: Some(effect.into()),
            path: Some(path.into()),
            at: Some(at.into()),
        }
    }
    pub fn budget_exceeded(declared: u64, ceiling: u64) -> Self {
        Self {
            kind: "budget_exceeded".into(),
            detail: format!("declared budget {declared} exceeds ceiling {ceiling}"),
            effect: Some("budget".into()),
            path: None,
            at: None,
        }
    }
}

/// Walk the program's declared effects (gathered from fn signatures) and
/// verify them against `policy`. Run before any execution.
pub fn check_program(program: &Program, policy: &Policy) -> Result<PolicyReport, Vec<PolicyViolation>> {
    let mut violations = Vec::new();
    let mut total_budget: u64 = 0;
    let mut declared_effects: IndexMap<String, Vec<DeclaredEffect>> = IndexMap::new();

    for f in &program.functions {
        for e in &f.effects {
            declared_effects.entry(f.name.clone()).or_default().push(e.clone());

            // Effect-kind allowlist (#207). A grant like `mcp:ocpp`
            // permits `[mcp("ocpp")]` only; bare `mcp` permits any
            // `[mcp(...)]`. Subsumption follows the type-system rule
            // in `lex-types::EffectKind::subsumes`. The CLI wire
            // format stays plain strings for backward compat.
            if !is_effect_allowed(&policy.allow_effects, e) {
                violations.push(PolicyViolation::effect_not_allowed(
                    &declared_effect_pretty(e), &f.name));
                continue;
            }

            // Scoped fs paths.
            if e.kind == "fs_read" || e.kind == "fs_write" {
                if let Some(EffectArg::Str(path)) = &e.arg {
                    let allowlist = if e.kind == "fs_read" {
                        &policy.allow_fs_read
                    } else {
                        &policy.allow_fs_write
                    };
                    if !path_under_any(path, allowlist) {
                        violations.push(PolicyViolation::fs_path_not_allowed(&e.kind, path, &f.name));
                    }
                }
            }

            // Budget aggregation.
            if e.kind == "budget" {
                if let Some(EffectArg::Int(n)) = &e.arg {
                    if *n >= 0 { total_budget = total_budget.saturating_add(*n as u64); }
                }
            }
        }
    }

    if let Some(ceiling) = policy.budget {
        if total_budget > ceiling {
            violations.push(PolicyViolation::budget_exceeded(total_budget, ceiling));
        }
    }

    if violations.is_empty() {
        Ok(PolicyReport { declared_effects, total_budget })
    } else {
        Err(violations)
    }
}

#[derive(Debug, Clone)]
pub struct PolicyReport {
    pub declared_effects: IndexMap<String, Vec<DeclaredEffect>>,
    pub total_budget: u64,
}

fn path_under_any(p: &str, list: &[PathBuf]) -> bool {
    let candidate = Path::new(p);
    list.iter().any(|allowed| candidate.starts_with(allowed))
}

/// Render a `DeclaredEffect` for diagnostic output, matching the
/// `EffectKind::pretty` form used by the type checker (#207).
fn declared_effect_pretty(e: &DeclaredEffect) -> String {
    match &e.arg {
        None => e.kind.clone(),
        Some(EffectArg::Str(s)) => format!("{}(\"{}\")", e.kind, s),
        Some(EffectArg::Int(n)) => format!("{}({})", e.kind, n),
        Some(EffectArg::Ident(s)) => format!("{}({})", e.kind, s),
    }
}

/// Decide whether `e` is permitted by `grants` (#207).
///
/// Grant strings come from `--allow-effects` and may be either:
///   - `name`           (bare wildcard, accepts any arg)
///   - `name:arg`       (string-arg specific grant — the colon is
///     a CLI-friendly separator)
///   - `name(arg)`      (matches the canonical pretty form for
///     grants written by hand or copy-pasted from
///     error messages)
///
/// Bare absorbs specific; specific matches only an exactly-equal
/// string arg. Int/Ident args on the declaration side are accepted
/// only by their bare-name grants (no CLI form for them in v1 —
/// they're rare in practice and can be added later).
pub fn is_effect_allowed(grants: &BTreeSet<String>, e: &DeclaredEffect) -> bool {
    grants.iter().any(|g| grant_subsumes(g, e))
}

fn grant_subsumes(grant: &str, e: &DeclaredEffect) -> bool {
    // Accept three forms: "name", "name:arg", "name(arg)".
    let (g_name, g_arg) = parse_grant(grant);
    if g_name != e.kind { return false; }
    match (g_arg, &e.arg) {
        (None, _) => true,                                 // bare absorbs anything
        (Some(_), None) => false,                          // specific can't grant bare
        (Some(g), Some(EffectArg::Str(d))) => g == d,
        // Int / Ident args have no CLI form in v1; only bare grants
        // satisfy them (handled by the (None, _) branch above).
        (Some(_), Some(_)) => false,
    }
}

/// Split `"mcp:ocpp"` or `"mcp(ocpp)"` into `("mcp", Some("ocpp"))`.
/// Plain `"mcp"` returns `("mcp", None)`.
fn parse_grant(s: &str) -> (&str, Option<&str>) {
    if let Some((name, rest)) = s.split_once('(') {
        if let Some(arg) = rest.strip_suffix(')') {
            return (name, Some(arg.trim_matches('"')));
        }
    }
    if let Some((name, arg)) = s.split_once(':') {
        return (name, Some(arg));
    }
    (s, None)
}
