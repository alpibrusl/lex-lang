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
    pub budget: Option<u64>,
}

impl Policy {
    pub fn pure() -> Self { Self::default() }

    pub fn permissive() -> Self {
        let mut s = BTreeSet::new();
        for k in ["io", "net", "time", "rand", "llm", "proc", "panic", "fs_read", "fs_write", "budget"] {
            s.insert(k.to_string());
        }
        Self { allow_effects: s, allow_fs_read: Vec::new(), allow_fs_write: Vec::new(), budget: None }
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

            // Effect kind allowlist.
            if !policy.allow_effects.contains(&e.kind) {
                violations.push(PolicyViolation::effect_not_allowed(&e.kind, &f.name));
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
