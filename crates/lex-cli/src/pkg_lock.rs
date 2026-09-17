//! `lex.lock` — the resolved, reproducible pin file for registry
//! dependencies (#893).
//!
//! A `lex.toml` registry dependency states a *constraint*
//! (`{ registry = "…", version = "^1.2" }`); the registry publishes a set
//! of immutable releases (#911), each `name@MAJOR.MINOR.PATCH` mapped to a
//! single op-log head. Resolution picks the **highest** release satisfying
//! the constraint ([`lex_syntax::semver::best_match`]) and records the exact
//! choice — version *and* the op-log head it points at — in `lex.lock`, so a
//! later `lex pkg install` rebuilds the identical closure instead of drifting
//! to whatever is newest.
//!
//! Two commands drive this file:
//!
//! * `lex pkg lock` — resolve, but keep any existing pin that still satisfies
//!   its constraint (reproducible: a lock that is already valid does not move).
//! * `lex pkg update` — re-resolve every dependency to the highest match,
//!   discarding existing pins (deliberately takes newer releases).
//!
//! The HTTP surface is thin and lives here; the *decision* is a pure function
//! ([`resolve_one`]) so it is unit-tested without a network.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

// The lockfile *format* (`LockFile`/`LockEntry`) lives in `lex-syntax` next to
// the manifest and semver types, because package resolution reads it. This
// module owns the *policy* — which release to pick and how to fetch the
// version list — and re-exports the format for callers here.
pub use lex_syntax::lock::{LockEntry, LockFile, LOCK_FORMAT_VERSION};

/// A registry release as reported by `GET /v1/pkg/{name}/versions`: the
/// version string and the op-log head it resolves to (#911).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableRelease {
    pub version: String,
    pub head_op: Option<String>,
}

/// Resolve one dependency **purely**: pick the highest of `available` that
/// satisfies `constraint`, preferring to keep `existing` when it is still a
/// valid choice (reproducibility).
///
/// * `keep_existing` — `lex pkg lock` passes `true` (don't move a pin that
///   still satisfies its constraint); `lex pkg update` passes `false`
///   (always take the highest match).
///
/// Returns `Ok(None)` when nothing satisfies the constraint — the caller
/// reports which dependency could not resolve and against which release set,
/// rather than this function guessing.
pub fn resolve_one(
    name: &str,
    registry: &str,
    constraint: &str,
    available: &[AvailableRelease],
    existing: Option<&LockEntry>,
    keep_existing: bool,
) -> Option<LockEntry> {
    // Reproducible path: an existing pin that still satisfies its constraint
    // *and* is still an offered release stays put.
    if keep_existing {
        if let Some(prev) = existing {
            let still_offered = available.iter().any(|r| r.version == prev.version);
            if still_offered
                && prev.constraint == constraint
                && lex_syntax::semver::satisfies(constraint, &prev.version)
            {
                return Some(prev.clone());
            }
        }
    }

    let versions: Vec<String> = available.iter().map(|r| r.version.clone()).collect();
    let chosen = lex_syntax::semver::best_match(constraint, &versions)?;
    let head_op = available
        .iter()
        .find(|r| r.version == chosen)
        .and_then(|r| r.head_op.clone());
    Some(LockEntry {
        name: name.to_string(),
        registry: registry.to_string(),
        constraint: constraint.to_string(),
        version: chosen.to_string(),
        head_op,
    })
}

/// Fetch the release set for `name` from a registry base URL via
/// `GET {base}/v1/pkg/{name}/versions` (#911). The endpoint returns
/// `{"versions": [{"version": "1.2.0", "head_op": "op_…"}, …]}`.
pub fn fetch_versions(registry: &str, name: &str) -> Result<Vec<AvailableRelease>> {
    #[derive(Deserialize)]
    struct VersionsResp {
        #[serde(default)]
        versions: Vec<VersionSummary>,
    }
    #[derive(Deserialize)]
    struct VersionSummary {
        version: String,
        #[serde(default)]
        head_op: Option<String>,
    }

    // A tenant-qualified registry (`host/tenant[/store]`) reads the hub's
    // public, unauthenticated surface (#917); a bare host falls back to the
    // legacy `{registry}/v1/pkg/…` form.
    let url = match lex_syntax::registry::public(registry) {
        Some(pr) => pr.versions_url(name),
        None => {
            let base = registry.trim_end_matches('/');
            let base = if base.contains("://") {
                base.to_string()
            } else {
                format!("https://{base}")
            };
            format!("{base}/v1/pkg/{name}/versions")
        }
    };
    let body = match ureq::get(&url).call() {
        Ok(resp) => resp
            .into_body()
            .read_to_string()
            .map_err(|e| anyhow::anyhow!("reading versions for {name}: {e}"))?,
        Err(ureq::Error::StatusCode(404)) => {
            bail!("registry has no package {name:?} (GET {url} → 404)")
        }
        Err(e) => bail!("GET {url}: {e}"),
    };
    let parsed: VersionsResp =
        serde_json::from_str(&body).with_context(|| format!("parsing versions from {url}"))?;
    Ok(parsed
        .versions
        .into_iter()
        .map(|v| AvailableRelease {
            version: v.version,
            head_op: v.head_op,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str, head: Option<&str>) -> AvailableRelease {
        AvailableRelease {
            version: v.to_string(),
            head_op: head.map(str::to_string),
        }
    }

    #[test]
    fn resolves_highest_match() {
        let avail = vec![
            rel("1.0.0", Some("op_a")),
            rel("1.2.0", Some("op_b")),
            rel("1.4.9", Some("op_c")),
            rel("2.0.0", Some("op_d")),
        ];
        let e = resolve_one("lex-nt", "reg", "^1.2", &avail, None, false).unwrap();
        assert_eq!(e.version, "1.4.9");
        assert_eq!(e.head_op.as_deref(), Some("op_c"));
        assert_eq!(e.constraint, "^1.2");
    }

    #[test]
    fn no_match_is_none() {
        let avail = vec![rel("1.0.0", None), rel("1.4.0", None)];
        assert!(resolve_one("p", "reg", "^3", &avail, None, false).is_none());
    }

    #[test]
    fn lock_keeps_a_still_valid_existing_pin() {
        // Newer 1.4.9 exists, but the existing pin (1.2.0) still satisfies
        // ^1.2 — `lex pkg lock` must not move it.
        let avail = vec![rel("1.2.0", Some("op_b")), rel("1.4.9", Some("op_c"))];
        let existing = LockEntry {
            name: "p".into(),
            registry: "reg".into(),
            constraint: "^1.2".into(),
            version: "1.2.0".into(),
            head_op: Some("op_b".into()),
        };
        let kept = resolve_one("p", "reg", "^1.2", &avail, Some(&existing), true).unwrap();
        assert_eq!(kept.version, "1.2.0");
    }

    #[test]
    fn update_takes_the_highest_over_an_existing_pin() {
        let avail = vec![rel("1.2.0", Some("op_b")), rel("1.4.9", Some("op_c"))];
        let existing = LockEntry {
            name: "p".into(),
            registry: "reg".into(),
            constraint: "^1.2".into(),
            version: "1.2.0".into(),
            head_op: Some("op_b".into()),
        };
        // keep_existing = false ⇒ update semantics.
        let e = resolve_one("p", "reg", "^1.2", &avail, Some(&existing), false).unwrap();
        assert_eq!(e.version, "1.4.9");
    }

    #[test]
    fn lock_rerolls_when_the_constraint_changed() {
        // The lockfile pins 1.2.0 under ^1.2, but lex.toml now says ^1.4 —
        // the old pin no longer satisfies, so even `lock` must re-resolve.
        let avail = vec![rel("1.2.0", Some("op_b")), rel("1.4.9", Some("op_c"))];
        let existing = LockEntry {
            name: "p".into(),
            registry: "reg".into(),
            constraint: "^1.2".into(),
            version: "1.2.0".into(),
            head_op: Some("op_b".into()),
        };
        let e = resolve_one("p", "reg", "^1.4", &avail, Some(&existing), true).unwrap();
        assert_eq!(e.version, "1.4.9");
    }

    #[test]
    fn lock_rerolls_when_the_pinned_release_is_withdrawn() {
        // The pin points at 1.2.0, but the registry no longer offers it —
        // keep_existing must not resurrect a version that isn't available.
        let avail = vec![rel("1.3.0", Some("op_x")), rel("1.4.0", Some("op_y"))];
        let existing = LockEntry {
            name: "p".into(),
            registry: "reg".into(),
            constraint: "^1.2".into(),
            version: "1.2.0".into(),
            head_op: Some("op_b".into()),
        };
        let e = resolve_one("p", "reg", "^1.2", &avail, Some(&existing), true).unwrap();
        assert_eq!(e.version, "1.4.0");
    }

    #[test]
    fn lockfile_toml_round_trips_and_is_sorted() {
        let lf = LockFile {
            version: LOCK_FORMAT_VERSION,
            packages: vec![
                LockEntry {
                    name: "zeta".into(),
                    registry: "reg".into(),
                    constraint: "^1".into(),
                    version: "1.0.0".into(),
                    head_op: Some("op_z".into()),
                },
                LockEntry {
                    name: "alpha".into(),
                    registry: "reg".into(),
                    constraint: "^2".into(),
                    version: "2.1.0".into(),
                    head_op: None,
                },
            ],
        };
        let toml = lf.to_toml().unwrap();
        // Sorted: alpha's block precedes zeta's.
        assert!(toml.find("alpha").unwrap() < toml.find("zeta").unwrap());
        let back = LockFile::from_toml(&toml).unwrap();
        assert_eq!(back.version, LOCK_FORMAT_VERSION);
        assert_eq!(back.packages.len(), 2);
        // head_op None is omitted from TOML but parses back as None.
        let alpha = back.entry("alpha").unwrap();
        assert_eq!(alpha.head_op, None);
        assert_eq!(alpha.version, "2.1.0");
    }
}
