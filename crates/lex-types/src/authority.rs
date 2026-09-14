//! Authority derivation: the least [`Grant`] a program provably needs,
//! and what a change did to it.
//!
//! [`trust`](crate::trust) answers "does this effect fit that grant?".
//! This answers the other direction — **what grant does this code
//! require?** — which is a question only an effect system can answer
//! soundly. The declared effect rows are a static over-approximation of
//! every path through the body, and [`crate::check_program`] has already
//! rejected any row that lies about its body, so folding those rows into
//! a grant yields authority the program cannot exceed. A dynamic trace
//! reports what one run touched; this reports what every run could.
//!
//! Two consequences fall out, and both are processes rather than
//! checks:
//!
//! - **A sandbox can be derived instead of written.** The fold is
//!   minimal by construction — the level on a dimension is the join over
//!   the effects that touch it — so nothing in the result is there
//!   because someone was being careful. [`Authority::minimality_witness`]
//!   makes that checkable rather than claimed: for each dimension it
//!   names the effect that the next rank down would reject.
//! - **A change has an authority delta.** [`diff`] classifies two
//!   derivations as [`Verdict::Widening`], [`Verdict::Narrowing`] or
//!   [`Verdict::Unchanged`]. A source diff says what the code now does;
//!   this says what it may now *reach*, which is the reviewable form of
//!   the same change.
//!
//! ## The honest limits, stated once
//!
//! The trust lattice ranks three dimensions. Plenty of effects sit
//! outside it — `env`, `sql`, `approval`, `chat`, `kv` — because
//! [`effect_requirement`] maps them to no dimension, so *no* grant
//! refuses them. A grant-only comparison would therefore show nothing
//! when a program starts reading environment variables.
//! [`Authority::off_lattice`] and [`AuthorityDiff::off_lattice_added`]
//! report them separately: present in the review, while being honest
//! that no perimeter is what stops them.
//!
//! Network reach has a second limit. `std.net.get` carries a *bare*
//! `[net]` — its URL is a runtime value — so the type level binds no
//! host. [`Authority::unscoped_net`] records that, and the static answer
//! narrows to "may reach the network at all"; *which* host is a
//! perimeter question. Consumers must not narrow an egress allowlist
//! against a derivation carrying it.

use crate::trust::{
    effect_requirement, is_net_effect, Dimension, Grant, GrantId, Level, TrustError,
};
use crate::types::{EffectArg, EffectKind};
use crate::EffectSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The authority a program requires, derived from its own types.
///
/// A function of the source alone: the same source derives the same
/// `Authority`, and nothing in it comes from a policy file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    /// The least grant in the trust lattice that permits every declared
    /// effect. Minimal by construction — see
    /// [`Authority::minimality_witness`].
    pub grant: Grant,
    /// The minimal egress allowlist: every host a `net("host")` effect
    /// names, sorted.
    pub egress: Vec<String>,
    /// A bare `[net]` is present somewhere, so the set of hosts reached
    /// is not bound at the type level. Tracked separately because
    /// losing host precision is a widening even when the coarse network
    /// level does not move.
    pub unscoped_net: bool,
    /// Path scopes named by `fs_read` / `fs_walk` effects, sorted.
    pub fs_read: Vec<String>,
    /// Path scopes named by `fs_write` effects, sorted.
    pub fs_write: Vec<String>,
    /// Every declared effect kind, sorted — the lattice's and the rest.
    pub effects: Vec<String>,
    /// Declared effects [`effect_requirement`] maps to no dimension,
    /// sorted. No grant refuses these.
    pub off_lattice: Vec<String>,
}

impl Authority {
    /// Content address of the derived grant — a stable id for "this
    /// exact authority", so an approval can be bound to it.
    pub fn grant_id(&self) -> GrantId {
        self.grant.content_id()
    }

    /// Evidence that the derived grant is *tight*: for every dimension
    /// above `none`, the next level down rejects at least one declared
    /// effect.
    ///
    /// Computed rather than asserted, so a caller can print it and a
    /// test can check it. An empty vector means the grant is
    /// [`Grant::bottom`] — the program needs no authority at all, which
    /// is as tight as it gets.
    pub fn minimality_witness(&self, effects: &EffectSet) -> Vec<MinimalityWitness> {
        let mut out = Vec::new();
        for dim in Dimension::ALL {
            let level = self.grant.level(dim);
            let Some(lowered_to) = next_level_down(dim, level) else {
                continue; // already `none` on this dimension
            };
            let mut probe = self.grant;
            set_level(&mut probe, dim, lowered_to);
            if let Err(TrustError::EffectNotPermitted { effect, .. }) =
                probe.permits_effects(effects)
            {
                out.push(MinimalityWitness {
                    dimension: dim,
                    level,
                    lowered_to,
                    rejected_effect: effect,
                });
            }
        }
        out
    }
}

/// One dimension's proof that the derived level is not a rank too
/// generous: at `lowered_to`, `rejected_effect` no longer type-checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimalityWitness {
    pub dimension: Dimension,
    pub level: Level,
    pub lowered_to: Level,
    pub rejected_effect: String,
}

/// Set one dimension of a grant. Kept here rather than on [`Grant`]
/// because a grant with a dimension replaced is not necessarily a
/// meaningful grant — this is only ever used to build a probe that is
/// expected to fail.
fn set_level(g: &mut Grant, dim: Dimension, level: Level) {
    match dim {
        Dimension::Filesystem => g.filesystem = level,
        Dimension::Network => g.network = level,
        Dimension::Exec => g.exec = level,
    }
}

/// The level one rank below `level` on `dim`, or `None` if `level` is
/// already the bottom of that dimension's ladder.
pub fn next_level_down(dim: Dimension, level: Level) -> Option<Level> {
    let ladder = dim.levels();
    let idx = ladder.iter().position(|l| l.rank() == level.rank())?;
    idx.checked_sub(1).map(|i| ladder[i])
}

/// Derive the least authority an [`EffectSet`] requires.
///
/// Each effect names a dimension and the minimum level it needs; the
/// grant's level on a dimension is the join over every effect touching
/// it. Minimal by construction: drop any dimension a rank and the
/// effect that pushed it there stops being permitted.
///
/// The caller is responsible for having type-checked the program first
/// — a dishonest effect row makes every conclusion here unsound, which
/// is why nothing in this module parses.
pub fn derive_from_effects(effects: &EffectSet) -> Result<Authority, TrustError> {
    let (mut filesystem, mut network, mut exec) = (Level::None, Level::None, Level::None);
    let mut egress = BTreeSet::new();
    let mut fs_read = BTreeSet::new();
    let mut fs_write = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut off_lattice = BTreeSet::new();
    let mut unscoped_net = false;

    for e in &effects.concrete {
        kinds.insert(e.name.clone());
        match effect_requirement(&e.name) {
            Some((Dimension::Filesystem, required)) => filesystem = filesystem.join(required),
            Some((Dimension::Network, required)) => network = network.join(required),
            Some((Dimension::Exec, required)) => exec = exec.join(required),
            None => {
                off_lattice.insert(e.name.clone());
            }
        }
        if is_net_effect(&e.name) {
            match scope_arg(e) {
                Some(host) => {
                    egress.insert(host.to_string());
                }
                None => unscoped_net = true,
            }
        }
        match (e.name.as_str(), scope_arg(e)) {
            ("fs_read" | "fs_walk", Some(p)) => {
                fs_read.insert(p.to_string());
            }
            ("fs_write", Some(p)) => {
                fs_write.insert(p.to_string());
            }
            _ => {}
        }
    }

    Ok(Authority {
        grant: Grant::try_new(filesystem, network, exec)?,
        egress: egress.into_iter().collect(),
        unscoped_net,
        fs_read: fs_read.into_iter().collect(),
        fs_write: fs_write.into_iter().collect(),
        effects: kinds.into_iter().collect(),
        off_lattice: off_lattice.into_iter().collect(),
    })
}

/// The string argument of a scoped effect (`net("host")`,
/// `fs_read("/path")`), if it has one.
fn scope_arg(e: &EffectKind) -> Option<&str> {
    match &e.arg {
        Some(EffectArg::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

// ---------------------------------------------------------------- diff

/// How a change moved a program's authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The head needs exactly what the base needed.
    Unchanged,
    /// The head needs strictly less. Safe to apply without asking: the
    /// type checker has proved the removed authority unreachable.
    Narrowing,
    /// The head reaches somewhere the base could not. Any widening
    /// dominates any narrowing in the same change.
    Widening,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Unchanged => "unchanged",
            Verdict::Narrowing => "narrowing",
            Verdict::Widening => "widening",
        }
    }
}

/// One dimension's movement between two derivations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DimensionDelta {
    pub dimension: Dimension,
    pub from: Level,
    pub to: Level,
}

impl DimensionDelta {
    pub fn widens(&self) -> bool {
        self.to.rank() > self.from.rank()
    }
}

/// The authority delta between two versions of a program — the artifact
/// a reviewer reads next to the source diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityDiff {
    /// Dimensions whose level moved, in [`Dimension::ALL`] order.
    pub dimensions: Vec<DimensionDelta>,
    pub egress_added: Vec<String>,
    pub egress_removed: Vec<String>,
    pub fs_read_added: Vec<String>,
    pub fs_read_removed: Vec<String>,
    pub fs_write_added: Vec<String>,
    pub fs_write_removed: Vec<String>,
    /// Effect kinds outside the trust lattice the head declares and the
    /// base did not. No grant refuses these, which is exactly why a
    /// review should see them.
    pub off_lattice_added: Vec<String>,
    pub off_lattice_removed: Vec<String>,
    /// The head carries a bare `[net]` where the base's network reach
    /// was real and fully host-scoped: the same coarse level, less
    /// static precision, so it counts as a widening.
    pub lost_net_precision: bool,
    pub verdict: Verdict,
}

impl AuthorityDiff {
    /// True when nothing moved at all.
    pub fn is_empty(&self) -> bool {
        self.verdict == Verdict::Unchanged
    }
}

/// Compare two derivations.
pub fn diff(base: &Authority, head: &Authority) -> AuthorityDiff {
    let mut dimensions = Vec::new();
    for dim in Dimension::ALL {
        let (from, to) = (base.grant.level(dim), head.grant.level(dim));
        if from.rank() != to.rank() {
            dimensions.push(DimensionDelta {
                dimension: dim,
                from,
                to,
            });
        }
    }

    let egress_added = added(&base.egress, &head.egress);
    let egress_removed = added(&head.egress, &base.egress);
    let fs_read_added = added(&base.fs_read, &head.fs_read);
    let fs_read_removed = added(&head.fs_read, &base.fs_read);
    let fs_write_added = added(&base.fs_write, &head.fs_write);
    let fs_write_removed = added(&head.fs_write, &base.fs_write);
    let off_lattice_added = added(&base.off_lattice, &head.off_lattice);
    let off_lattice_removed = added(&head.off_lattice, &base.off_lattice);
    // Only a *loss* of precision counts: the base must have had network
    // reach, and had it fully bound to hosts. No network at all → a bare
    // `[net]` is already reported as a dimension widening, and saying it
    // twice would read as two findings.
    let lost_net_precision = head.unscoped_net && !base.unscoped_net && !base.egress.is_empty();

    let widens = dimensions.iter().any(DimensionDelta::widens)
        || !egress_added.is_empty()
        || !fs_read_added.is_empty()
        || !fs_write_added.is_empty()
        || !off_lattice_added.is_empty()
        || lost_net_precision;
    let narrows = dimensions.iter().any(|d| !d.widens())
        || !egress_removed.is_empty()
        || !fs_read_removed.is_empty()
        || !fs_write_removed.is_empty()
        || !off_lattice_removed.is_empty()
        || (base.unscoped_net && !head.unscoped_net);

    let verdict = if widens {
        Verdict::Widening
    } else if narrows {
        Verdict::Narrowing
    } else {
        Verdict::Unchanged
    };

    AuthorityDiff {
        dimensions,
        egress_added,
        egress_removed,
        fs_read_added,
        fs_read_removed,
        fs_write_added,
        fs_write_removed,
        off_lattice_added,
        off_lattice_removed,
        lost_net_precision,
        verdict,
    }
}

/// Entries of `b` not present in `a`.
fn added(a: &[String], b: &[String]) -> Vec<String> {
    let have: BTreeSet<&str> = a.iter().map(String::as_str).collect();
    b.iter()
        .filter(|x| !have.contains(x.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effects(rows: &[(&str, Option<&str>)]) -> EffectSet {
        let mut set = EffectSet::empty();
        for (name, arg) in rows {
            let kind = match arg {
                Some(v) => EffectKind::with_str(name.to_string(), v.to_string()),
                None => EffectKind::bare(name.to_string()),
            };
            set.concrete.insert(kind);
        }
        set
    }

    #[test]
    fn pure_code_needs_nothing() {
        let e = effects(&[]);
        let a = derive_from_effects(&e).unwrap();
        assert_eq!(a.grant, Grant::bottom());
        assert!(a.minimality_witness(&e).is_empty());
    }

    #[test]
    fn off_lattice_effects_claim_no_dimension() {
        // `io`, `env` and `sql` rank nowhere: no grant refuses them, so
        // inventing filesystem authority for them would be over-broad.
        let e = effects(&[("io", None), ("env", None), ("sql", None)]);
        let a = derive_from_effects(&e).unwrap();
        assert_eq!(a.grant, Grant::bottom());
        assert_eq!(a.off_lattice, vec!["env", "io", "sql"]);
        Grant::bottom().permits_effects(&e).expect("permitted");
    }

    #[test]
    fn the_level_is_the_join_over_the_effects_touching_a_dimension() {
        // fs_read alone → read-only; adding fs_write raises it, and
        // nothing lowers it back.
        let read = derive_from_effects(&effects(&[("fs_read", None)])).unwrap();
        assert_eq!(read.grant.filesystem, Level::ReadOnly);
        let both = derive_from_effects(&effects(&[("fs_read", None), ("fs_write", None)])).unwrap();
        assert_eq!(both.grant.filesystem, Level::ReadWrite);
    }

    /// The claim the whole process rests on.
    #[test]
    fn derived_grants_are_minimal() {
        for rows in [
            vec![],
            vec![("io", None)],
            vec![("fs_read", Some("/etc/hosts"))],
            vec![("fs_write", Some("/tmp/out")), ("net", None)],
            vec![("proc", None), ("llm_cloud", None)],
            vec![("net", Some("api.example.com")), ("fs_walk", None)],
        ] {
            let e = effects(&rows);
            let a = derive_from_effects(&e).unwrap();
            a.grant
                .permits_effects(&e)
                .expect("permits what it derived");
            for dim in Dimension::ALL {
                if let Some(lower) = next_level_down(dim, a.grant.level(dim)) {
                    let mut probe = a.grant;
                    set_level(&mut probe, dim, lower);
                    assert!(
                        probe.permits_effects(&e).is_err(),
                        "{dim} at {} is a rank too generous for {rows:?}",
                        a.grant.level(dim)
                    );
                }
            }
            let non_none = Dimension::ALL
                .iter()
                .filter(|d| a.grant.level(**d) != Level::None)
                .count();
            assert_eq!(a.minimality_witness(&e).len(), non_none);
        }
    }

    #[test]
    fn scopes_are_collected_per_kind() {
        let a = derive_from_effects(&effects(&[
            ("net", Some("a.example")),
            ("net", Some("b.example")),
            ("fs_read", Some("/in")),
            ("fs_write", Some("/out")),
        ]))
        .unwrap();
        assert_eq!(a.egress, vec!["a.example", "b.example"]);
        assert_eq!(a.fs_read, vec!["/in"]);
        assert_eq!(a.fs_write, vec!["/out"]);
        assert!(!a.unscoped_net, "every net effect named a host");
    }

    #[test]
    fn a_bare_net_is_recorded_as_unscoped() {
        let a = derive_from_effects(&effects(&[("net", None)])).unwrap();
        assert!(a.unscoped_net);
        assert!(a.egress.is_empty());
    }

    #[test]
    fn adding_a_host_is_a_widening_and_removing_one_is_a_narrowing() {
        let one = derive_from_effects(&effects(&[("net", Some("a.example"))])).unwrap();
        let two = derive_from_effects(&effects(&[
            ("net", Some("a.example")),
            ("net", Some("b.example")),
        ]))
        .unwrap();
        assert_eq!(diff(&one, &two).verdict, Verdict::Widening);
        assert_eq!(diff(&two, &one).verdict, Verdict::Narrowing);
        assert_eq!(diff(&one, &one).verdict, Verdict::Unchanged);
    }

    #[test]
    fn losing_host_precision_is_a_widening_even_at_the_same_level() {
        let scoped = derive_from_effects(&effects(&[("net", Some("a.example"))])).unwrap();
        let bare =
            derive_from_effects(&effects(&[("net", Some("a.example")), ("net", None)])).unwrap();
        assert_eq!(scoped.grant, bare.grant, "same coarse level");
        let d = diff(&scoped, &bare);
        assert!(d.lost_net_precision);
        assert_eq!(d.verdict, Verdict::Widening);
    }

    #[test]
    fn no_network_to_a_bare_net_is_not_reported_twice() {
        let none = derive_from_effects(&effects(&[])).unwrap();
        let bare = derive_from_effects(&effects(&[("net", None)])).unwrap();
        let d = diff(&none, &bare);
        assert_eq!(d.verdict, Verdict::Widening);
        assert!(
            !d.lost_net_precision,
            "the dimension widening already says it"
        );
    }

    #[test]
    fn a_widening_dominates_a_narrowing_in_the_same_change() {
        let base = derive_from_effects(&effects(&[("fs_write", None)])).unwrap();
        let head = derive_from_effects(&effects(&[("fs_read", None), ("net", None)])).unwrap();
        let d = diff(&base, &head);
        assert!(d.dimensions.iter().any(|x| !x.widens()), "filesystem fell");
        assert!(d.dimensions.iter().any(|x| x.widens()), "network rose");
        assert_eq!(d.verdict, Verdict::Widening);
    }

    #[test]
    fn off_lattice_movement_is_reported_though_no_grant_would_catch_it() {
        let base = derive_from_effects(&effects(&[])).unwrap();
        let head = derive_from_effects(&effects(&[("env", None)])).unwrap();
        let d = diff(&base, &head);
        assert_eq!(d.verdict, Verdict::Widening);
        assert_eq!(d.off_lattice_added, vec!["env"]);
        assert!(d.dimensions.is_empty(), "no dimension moved");
    }
}
