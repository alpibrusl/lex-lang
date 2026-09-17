//! Minimal semver **constraint** matching for `{ registry = …, version =
//! "…" }` dependency resolution (#893).
//!
//! Not a full semver implementation — no pre-release or build-metadata
//! ordering — but enough to resolve `^`, `~`, exact, and comparator
//! (`>= > <= <`) constraints against a set of published
//! `MAJOR.MINOR.PATCH` release versions. Resolution is **deterministic**:
//! [`best_match`] always returns the *highest* satisfying version, so a
//! `lex.lock` pins a reproducible choice.
//!
//! Caret/tilde follow Cargo's rules (the left-most non-zero component is
//! the "fixed" one for `^`; `~` fixes down to the last given component).

/// A `(major, minor, patch)` version.
pub type Version = (u64, u64, u64);

/// Parse a full `MAJOR.MINOR.PATCH` (a leading `v` allowed). Exactly
/// three components, unlike [`parse_partial`].
pub fn parse_exact(v: &str) -> Option<Version> {
    let (ver, given) = parse_partial(v)?;
    if given == 3 {
        Some(ver)
    } else {
        None
    }
}

/// Parse `MAJOR[.MINOR[.PATCH]]` (leading `v` allowed); omitted
/// components default to 0. Returns the version and how many components
/// were actually written (1..=3) — caret/tilde need that.
fn parse_partial(s: &str) -> Option<(Version, u8)> {
    let s = s.trim().trim_start_matches('v').trim();
    if s.is_empty() {
        return None;
    }
    let mut parts = s.split('.');
    let major: u64 = parts.next()?.trim().parse().ok()?;
    let (minor, has_minor) = match parts.next() {
        Some(m) => (m.trim().parse().ok()?, true),
        None => (0, false),
    };
    let (patch, has_patch) = match parts.next() {
        Some(p) => (p.trim().parse().ok()?, true),
        None => (0, false),
    };
    if parts.next().is_some() {
        return None; // more than three components
    }
    Some(((major, minor, patch), 1 + has_minor as u8 + has_patch as u8))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Ge,
    Gt,
    Le,
    Lt,
}

#[derive(Debug, Clone)]
enum Constraint {
    /// `*`, `latest`, or empty — any released version.
    Any,
    /// `1.2.3` — exactly this version.
    Exact(Version),
    /// `^X.Y.Z` — `>= v` and `< caret_upper(v, given)`.
    Caret(Version, Version),
    /// `~X.Y.Z` — `>= v` and `< tilde_upper(v, given)`.
    Tilde(Version, Version),
    /// A single comparator, e.g. `>=1.0`.
    Compare(Cmp, Version),
}

/// The exclusive upper bound of a caret constraint: increment the
/// left-most non-zero component (rest zeroed); if every given component
/// is zero, increment the last *given* component.
fn caret_upper((maj, min, pat): Version, given: u8) -> Version {
    if maj > 0 {
        (maj + 1, 0, 0)
    } else if min > 0 {
        (0, min + 1, 0)
    } else if pat > 0 {
        (0, 0, pat + 1)
    } else {
        match given {
            1 => (1, 0, 0),
            2 => (0, 1, 0),
            _ => (0, 0, 1),
        }
    }
}

/// The exclusive upper bound of a tilde constraint: minor given ⇒ fix the
/// minor (`< maj.(min+1).0`); only major given ⇒ fix the major.
fn tilde_upper((maj, min, _pat): Version, given: u8) -> Version {
    if given >= 2 {
        (maj, min + 1, 0)
    } else {
        (maj + 1, 0, 0)
    }
}

fn parse_constraint(s: &str) -> Option<Constraint> {
    let s = s.trim();
    if s.is_empty() || s == "*" || s.eq_ignore_ascii_case("latest") {
        return Some(Constraint::Any);
    }
    if let Some(rest) = s.strip_prefix('^') {
        let (v, given) = parse_partial(rest)?;
        return Some(Constraint::Caret(v, caret_upper(v, given)));
    }
    if let Some(rest) = s.strip_prefix('~') {
        let (v, given) = parse_partial(rest)?;
        return Some(Constraint::Tilde(v, tilde_upper(v, given)));
    }
    for (pfx, cmp) in [(">=", Cmp::Ge), ("<=", Cmp::Le), (">", Cmp::Gt), ("<", Cmp::Lt)] {
        if let Some(rest) = s.strip_prefix(pfx) {
            return Some(Constraint::Compare(cmp, parse_partial(rest)?.0));
        }
    }
    // A bare version with `=` is exact; a bare `1.2` is treated as `^1.2`
    // (Cargo's default), which is the friendlier "compatible" reading.
    if let Some(rest) = s.strip_prefix('=') {
        return Some(Constraint::Exact(parse_partial(rest)?.0));
    }
    let (v, given) = parse_partial(s)?;
    if given == 3 {
        Some(Constraint::Exact(v))
    } else {
        Some(Constraint::Caret(v, caret_upper(v, given)))
    }
}

/// Does `version` (a `MAJOR.MINOR.PATCH` string) satisfy `constraint`?
/// An unparseable version never matches; an unparseable constraint
/// matches nothing (the caller should surface "cannot resolve" rather
/// than guess).
pub fn satisfies(constraint: &str, version: &str) -> bool {
    let (Some(c), Some(v)) = (parse_constraint(constraint), parse_exact(version)) else {
        return false;
    };
    match c {
        Constraint::Any => true,
        Constraint::Exact(e) => v == e,
        Constraint::Caret(lo, hi) | Constraint::Tilde(lo, hi) => v >= lo && v < hi,
        Constraint::Compare(Cmp::Ge, r) => v >= r,
        Constraint::Compare(Cmp::Gt, r) => v > r,
        Constraint::Compare(Cmp::Le, r) => v <= r,
        Constraint::Compare(Cmp::Lt, r) => v < r,
    }
}

/// The highest of `versions` that satisfies `constraint`, or `None` if
/// none do. Deterministic — the resolver records this exact version in
/// the lock file.
pub fn best_match<'a>(constraint: &str, versions: &'a [String]) -> Option<&'a str> {
    versions
        .iter()
        .filter(|v| satisfies(constraint, v))
        .max_by_key(|v| parse_exact(v).unwrap_or((0, 0, 0)))
        .map(|s| s.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact() {
        assert!(satisfies("1.2.3", "1.2.3"));
        assert!(satisfies("=1.2.3", "1.2.3"));
        assert!(!satisfies("1.2.3", "1.2.4"));
    }

    #[test]
    fn caret_major() {
        assert!(satisfies("^1.2.3", "1.2.3"));
        assert!(satisfies("^1.2.3", "1.9.0"));
        assert!(!satisfies("^1.2.3", "2.0.0"));
        assert!(!satisfies("^1.2.3", "1.2.2"));
        // partial: ^1.2 == >=1.2.0 <2.0.0
        assert!(satisfies("^1.2", "1.2.0"));
        assert!(satisfies("^1.2", "1.5.9"));
        assert!(!satisfies("^1.2", "2.0.0"));
        assert!(!satisfies("^1.2", "1.1.9"));
        // ^1 == >=1.0.0 <2.0.0
        assert!(satisfies("^1", "1.0.0") && satisfies("^1", "1.9.9") && !satisfies("^1", "2.0.0"));
        // bare 1.2 defaults to ^1.2
        assert!(satisfies("1.2", "1.4.0") && !satisfies("1.2", "2.0.0"));
    }

    #[test]
    fn caret_zero() {
        // ^0.2.3 == >=0.2.3 <0.3.0
        assert!(satisfies("^0.2.3", "0.2.3") && satisfies("^0.2.3", "0.2.9"));
        assert!(!satisfies("^0.2.3", "0.3.0") && !satisfies("^0.2.3", "0.2.2"));
        // ^0.0.3 == >=0.0.3 <0.0.4
        assert!(satisfies("^0.0.3", "0.0.3") && !satisfies("^0.0.3", "0.0.4"));
        // ^0.2 == >=0.2.0 <0.3.0
        assert!(satisfies("^0.2", "0.2.5") && !satisfies("^0.2", "0.3.0"));
        // ^0 == >=0.0.0 <1.0.0
        assert!(satisfies("^0", "0.9.9") && !satisfies("^0", "1.0.0"));
    }

    #[test]
    fn tilde() {
        // ~1.2.3 == >=1.2.3 <1.3.0
        assert!(satisfies("~1.2.3", "1.2.9") && !satisfies("~1.2.3", "1.3.0"));
        // ~1.2 == >=1.2.0 <1.3.0
        assert!(satisfies("~1.2", "1.2.9") && !satisfies("~1.2", "1.3.0"));
        // ~1 == >=1.0.0 <2.0.0
        assert!(satisfies("~1", "1.9.9") && !satisfies("~1", "2.0.0"));
    }

    #[test]
    fn comparators_and_any() {
        assert!(satisfies(">=1.0", "1.0.0") && satisfies(">=1.0", "9.9.9") && !satisfies(">=1.0", "0.9.9"));
        assert!(satisfies(">1.0", "1.0.1") && !satisfies(">1.0", "1.0.0"));
        assert!(satisfies("<2.0", "1.9.9") && !satisfies("<2.0", "2.0.0"));
        assert!(satisfies("<=1.5", "1.5.0") && !satisfies("<=1.5", "1.5.1"));
        for c in ["*", "latest", ""] {
            assert!(satisfies(c, "3.1.4"), "{c} should match anything");
        }
    }

    #[test]
    fn best_match_picks_highest() {
        let vs: Vec<String> = ["1.0.0", "1.2.0", "1.4.9", "2.0.0", "1.4.2"]
            .iter().map(|s| s.to_string()).collect();
        assert_eq!(best_match("^1.2", &vs), Some("1.4.9"));
        assert_eq!(best_match("~1.4", &vs), Some("1.4.9"));
        assert_eq!(best_match(">=1.0", &vs), Some("2.0.0"));
        assert_eq!(best_match("^3", &vs), None);
        assert_eq!(best_match("1.4.2", &vs), Some("1.4.2"));
    }

    #[test]
    fn unparseable_never_matches() {
        assert!(!satisfies("^1.2.3", "not-a-version"));
        assert!(!satisfies("garbage", "1.2.3"));
    }
}
