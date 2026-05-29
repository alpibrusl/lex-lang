//! Trust lattice: effect-narrowing as subtyping over a small fixed
//! dimension set (filesystem, network, exec).
//!
//! This is the type-level half of the lex-os trust model (see the
//! lex-os design doc §7). The key idea AgentSpec only *described* in a
//! prose "trust block", Lex makes a **type property**:
//!
//! - Trust dimensions form a product **lattice**.
//! - Manifest inheritance is **subtyping** over that lattice: a child
//!   grant may only *narrow* (be ≤) its parent. Widening is a type
//!   error, caught by construction rather than a hoped-for runtime
//!   check ([`Grant::narrow`]).
//! - The same grant that drives this static check also tells the
//!   supervisor what OS sandbox to derive — the effects a function
//!   uses ([`EffectSet`]) are checked against the grant with
//!   [`Grant::permits_effects`], so code that calls a `net` effect
//!   will not satisfy a `network: none` grant.
//!
//! The module is deliberately self-contained: it adds a lattice
//! primitive that is useful to *any* Lex program reasoning about
//! capabilities, not just the agent runtime, and it does not change
//! the behaviour of the existing checker.

use crate::types::{EffectKind, EffectSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// The three trust dimensions an effect can touch. Kept deliberately
/// small and fixed (design doc §7.2): every consequential effect a box
/// can have on the world reduces to filesystem reach, network reach, or
/// the ability to spawn arbitrary executables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Dimension {
    Filesystem,
    Network,
    Exec,
}

impl Dimension {
    pub const ALL: [Dimension; 3] = [Dimension::Filesystem, Dimension::Network, Dimension::Exec];

    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Filesystem => "filesystem",
            Dimension::Network => "network",
            Dimension::Exec => "exec",
        }
    }
}

impl fmt::Display for Dimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Trust level along a single dimension. Levels are **totally ordered**
/// from `None` (no authority) upward; the numeric discriminant *is* the
/// order, so `<=`/`max`/`min` on the rank give the lattice operations.
///
/// The levels are shared across dimensions (a deliberately small
/// vocabulary) but not every level is meaningful on every dimension —
/// the canonical readings are:
///
/// | rank | Filesystem | Network   | Exec       |
/// |------|------------|-----------|------------|
/// | 0    | none       | none      | none       |
/// | 1    | read-only  | loopback  | sandboxed  |
/// | 2    | read-write | allowlist | (= full)   |
/// | 3    | full       | full      | full       |
///
/// `Sandboxed` aliases rank 1 for exec; `Allowlist` aliases rank 2 for
/// network. They are distinct enum variants for legibility but compare
/// purely by [`Level::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Level {
    /// rank 0 — the effect is *physically absent* from the box.
    None,
    /// rank 1 — read-only / loopback-only / sandboxed-exec.
    ReadOnly,
    /// rank 1 — exec spelled for legibility (same rank as `ReadOnly`).
    Sandboxed,
    /// rank 1 — network loopback only.
    Loopback,
    /// rank 2 — read-write filesystem.
    ReadWrite,
    /// rank 2 — network restricted to an allowlist.
    Allowlist,
    /// rank 3 — unrestricted authority on the dimension.
    Full,
}

impl Level {
    /// The position of this level in the total order. Lattice
    /// operations are defined on the rank.
    pub fn rank(self) -> u8 {
        match self {
            Level::None => 0,
            Level::ReadOnly | Level::Sandboxed | Level::Loopback => 1,
            Level::ReadWrite | Level::Allowlist => 2,
            Level::Full => 3,
        }
    }

    /// `self` ≤ `other` in the trust order (self grants no more than
    /// other). This is the per-dimension subtyping relation.
    pub fn leq(self, other: Level) -> bool {
        self.rank() <= other.rank()
    }

    /// Least upper bound (join): the tighter of two levels that still
    /// covers both. Returns the higher-ranked level.
    pub fn join(self, other: Level) -> Level {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }

    /// Greatest lower bound (meet): the most authority both allow.
    /// Returns the lower-ranked level.
    pub fn meet(self, other: Level) -> Level {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::ReadOnly => "read-only",
            Level::Sandboxed => "sandboxed",
            Level::Loopback => "loopback",
            Level::ReadWrite => "read-write",
            Level::Allowlist => "allowlist",
            Level::Full => "full",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A capability grant: one [`Level`] per [`Dimension`]. This is the
/// trust manifest's core payload. As a product of totally-ordered
/// dimensions it forms a **lattice** under componentwise ordering, with
/// [`Grant::bottom`] (deny everything) and [`Grant::top`] (the most
/// dangerous config — `sudo` + open internet, design doc §3) as the
/// extremes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub filesystem: Level,
    pub network: Level,
    pub exec: Level,
}

/// Why a requested grant was refused. The runtime contract is
/// *refuse, don't downgrade* (design doc §7.5): when a child manifest
/// asks for more than its parent allows we return this error rather
/// than silently clamping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error(
        "trust widening on {dimension}: child requests `{requested}` but parent only grants `{parent}` (a child manifest may only narrow)"
    )]
    Widens {
        dimension: Dimension,
        parent: Level,
        requested: Level,
    },
    #[error(
        "effect `{effect}` needs {dimension} ≥ `{required}` but the grant only provides `{granted}`"
    )]
    EffectNotPermitted {
        effect: String,
        dimension: Dimension,
        required: Level,
        granted: Level,
    },
}

impl Grant {
    pub fn new(filesystem: Level, network: Level, exec: Level) -> Self {
        Self { filesystem, network, exec }
    }

    /// Deny everything — the lattice bottom. The default starting point
    /// for the narrowest-possible grant (design doc §5.1): every
    /// ungranted effect is physically absent.
    pub fn bottom() -> Self {
        Self::new(Level::None, Level::None, Level::None)
    }

    /// Grant everything — the lattice top. `sudo` + open internet; the
    /// single most dangerous config. Never the default.
    pub fn top() -> Self {
        Self::new(Level::Full, Level::Full, Level::Full)
    }

    pub fn level(&self, dim: Dimension) -> Level {
        match dim {
            Dimension::Filesystem => self.filesystem,
            Dimension::Network => self.network,
            Dimension::Exec => self.exec,
        }
    }

    /// `self` ≤ `other`: self grants no more authority than other on
    /// *any* dimension. This is the subtyping relation over the trust
    /// lattice — a narrower grant is a subtype of a wider one.
    pub fn leq(&self, other: &Grant) -> bool {
        Dimension::ALL
            .iter()
            .all(|&d| self.level(d).leq(other.level(d)))
    }

    /// Componentwise join (least upper bound).
    pub fn join(&self, other: &Grant) -> Grant {
        Grant::new(
            self.filesystem.join(other.filesystem),
            self.network.join(other.network),
            self.exec.join(other.exec),
        )
    }

    /// Componentwise meet (greatest lower bound).
    pub fn meet(&self, other: &Grant) -> Grant {
        Grant::new(
            self.filesystem.meet(other.filesystem),
            self.network.meet(other.network),
            self.exec.meet(other.exec),
        )
    }

    /// Narrowing-as-subtyping (design doc §7.1, "the narrowing
    /// invariant becomes a type property"). A child manifest is only
    /// well-formed if it narrows its parent on every dimension; any
    /// widening is rejected here — the inheritance equivalent of a
    /// type error. On success returns the (validated) child grant.
    pub fn narrow(parent: &Grant, child: &Grant) -> Result<Grant, TrustError> {
        for &d in &Dimension::ALL {
            let p = parent.level(d);
            let c = child.level(d);
            if !c.leq(p) {
                return Err(TrustError::Widens {
                    dimension: d,
                    parent: p,
                    requested: c,
                });
            }
        }
        Ok(*child)
    }

    /// Does this grant permit a single effect? Effects are mapped to a
    /// dimension and the minimum level they require via
    /// [`effect_requirement`]; effects outside the trust vocabulary
    /// (pure compute, logging, time, rng) are always permitted.
    pub fn permits_effect(&self, effect: &EffectKind) -> bool {
        match effect_requirement(&effect.name) {
            Some((dim, required)) => required.leq(self.level(dim)),
            None => true,
        }
    }

    /// Check every concrete effect in a set against the grant. This is
    /// the bridge that makes "code calling a `net` effect won't
    /// type-check under a `network: none` grant" true (design doc §7).
    /// Returns the first offending effect as a [`TrustError`].
    pub fn permits_effects(&self, effects: &EffectSet) -> Result<(), TrustError> {
        for e in &effects.concrete {
            if let Some((dim, required)) = effect_requirement(&e.name) {
                let granted = self.level(dim);
                if !required.leq(granted) {
                    return Err(TrustError::EffectNotPermitted {
                        effect: e.pretty(),
                        dimension: dim,
                        required,
                        granted,
                    });
                }
            }
        }
        Ok(())
    }

    /// Canonical one-line rendering, e.g.
    /// `fs=read-only net=none exec=none`.
    pub fn pretty(&self) -> String {
        format!(
            "fs={} net={} exec={}",
            self.filesystem, self.network, self.exec
        )
    }

    /// Content-addressed identity of the grant. The bytes hashed are a
    /// stable canonical form (dimension order is fixed, ranks not enum
    /// names), so a `GrantId` is reproducible across processes and
    /// languages — the manifest stays hashable exactly as AgentSpec
    /// required (design doc §7.4). Two grants with the same authority
    /// hash identically even if spelled with different aliases
    /// (`Sandboxed` vs `ReadOnly`).
    pub fn content_id(&self) -> GrantId {
        let mut hasher = Sha256::new();
        hasher.update(b"lex.trust.grant.v1");
        for &d in &Dimension::ALL {
            hasher.update([d as u8, self.level(d).rank()]);
        }
        let digest = hasher.finalize();
        GrantId(hex::encode(digest))
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.pretty())
    }
}

/// Content address of a [`Grant`] — a hex-encoded SHA-256 of its
/// canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GrantId(pub String);

impl GrantId {
    /// Short form for logs/diagnostics (first 12 hex chars).
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }
}

impl fmt::Display for GrantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "grant:{}", self.short())
    }
}

/// Map a Lex effect name to the trust dimension it touches and the
/// minimum [`Level`] required to use it. Effects not listed are pure or
/// otherwise outside the trust model and need no grant.
///
/// Keep this aligned with the builtin effect names in
/// `crates/lex-types/src/builtins.rs`.
pub fn effect_requirement(effect_name: &str) -> Option<(Dimension, Level)> {
    use Dimension::*;
    use Level::*;
    match effect_name {
        // Filesystem reach.
        "fs_read" | "fs_walk" => Some((Filesystem, ReadOnly)),
        "fs_write" => Some((Filesystem, ReadWrite)),
        // Network egress. Any of these needs at least allowlisted net;
        // a `network: none` or `loopback` grant rejects them.
        "net" | "http" | "mcp" | "llm_cloud" => Some((Network, Allowlist)),
        // Arbitrary process execution.
        "proc" => Some((Exec, Sandboxed)),
        // Pure / harmless effects (io, log, time, rand, kv, env, ...)
        // are outside the trust model.
        _ => Option::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_total_order() {
        assert!(Level::None.leq(Level::ReadOnly));
        assert!(Level::ReadOnly.leq(Level::ReadWrite));
        assert!(Level::ReadWrite.leq(Level::Full));
        assert!(!Level::Full.leq(Level::ReadOnly));
        // Aliases at the same rank compare equal-ish.
        assert!(Level::Sandboxed.leq(Level::ReadOnly));
        assert!(Level::ReadOnly.leq(Level::Sandboxed));
        assert!(Level::Loopback.leq(Level::ReadOnly));
    }

    #[test]
    fn level_join_meet() {
        assert_eq!(Level::None.join(Level::Full).rank(), Level::Full.rank());
        assert_eq!(Level::None.meet(Level::Full).rank(), Level::None.rank());
        assert_eq!(
            Level::ReadOnly.join(Level::ReadWrite).rank(),
            Level::ReadWrite.rank()
        );
        assert_eq!(
            Level::ReadOnly.meet(Level::ReadWrite).rank(),
            Level::ReadOnly.rank()
        );
    }

    #[test]
    fn grant_lattice_extremes() {
        let b = Grant::bottom();
        let t = Grant::top();
        assert!(b.leq(&t));
        assert!(!t.leq(&b));
        // bottom is the identity for join, top for meet.
        let g = Grant::new(Level::ReadOnly, Level::Loopback, Level::None);
        assert_eq!(b.join(&g), g);
        assert_eq!(t.meet(&g), g);
    }

    #[test]
    fn narrowing_allowed() {
        let parent = Grant::new(Level::ReadWrite, Level::Full, Level::Sandboxed);
        let child = Grant::new(Level::ReadOnly, Level::None, Level::None);
        assert_eq!(Grant::narrow(&parent, &child), Ok(child));
    }

    #[test]
    fn widening_is_rejected() {
        let parent = Grant::new(Level::ReadOnly, Level::None, Level::None);
        // Child tries to widen network none -> full.
        let child = Grant::new(Level::ReadOnly, Level::Full, Level::None);
        let err = Grant::narrow(&parent, &child).unwrap_err();
        assert_eq!(
            err,
            TrustError::Widens {
                dimension: Dimension::Network,
                parent: Level::None,
                requested: Level::Full,
            }
        );
    }

    #[test]
    fn narrowing_is_transitive_via_leq() {
        let a = Grant::top();
        let b = Grant::new(Level::ReadWrite, Level::Loopback, Level::None);
        let c = Grant::new(Level::ReadOnly, Level::None, Level::None);
        assert!(Grant::narrow(&a, &b).is_ok());
        assert!(Grant::narrow(&b, &c).is_ok());
        // …and the chain composes: c narrows a directly.
        assert!(Grant::narrow(&a, &c).is_ok());
    }

    #[test]
    fn effect_permitted_under_matching_grant() {
        let read_only = Grant::new(Level::ReadOnly, Level::None, Level::None);
        assert!(read_only.permits_effect(&EffectKind::bare("fs_read")));
        // fs_write needs ReadWrite, denied under ReadOnly.
        assert!(!read_only.permits_effect(&EffectKind::bare("fs_write")));
        // net denied under network: none.
        assert!(!read_only.permits_effect(&EffectKind::bare("net")));
        // pure effects always allowed.
        assert!(read_only.permits_effect(&EffectKind::bare("log")));
        assert!(read_only.permits_effect(&EffectKind::bare("time")));
    }

    #[test]
    fn effect_set_checked_against_grant() {
        // The headline guarantee: a function that uses `net` does not
        // satisfy a `network: none` grant.
        let analyze_grant = Grant::new(Level::ReadOnly, Level::None, Level::None);
        let mut effects = EffectSet::empty();
        effects.concrete.insert(EffectKind::bare("fs_read"));
        effects.concrete.insert(EffectKind::with_str("net", "evil.example"));
        let err = analyze_grant.permits_effects(&effects).unwrap_err();
        match err {
            TrustError::EffectNotPermitted { dimension, required, granted, .. } => {
                assert_eq!(dimension, Dimension::Network);
                assert_eq!(required, Level::Allowlist);
                assert_eq!(granted, Level::None);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn effect_set_fully_within_grant_ok() {
        let grant = Grant::new(Level::ReadWrite, Level::Full, Level::Sandboxed);
        let mut effects = EffectSet::empty();
        effects.concrete.insert(EffectKind::bare("fs_read"));
        effects.concrete.insert(EffectKind::bare("fs_write"));
        effects.concrete.insert(EffectKind::bare("net"));
        effects.concrete.insert(EffectKind::bare("proc"));
        assert!(grant.permits_effects(&effects).is_ok());
    }

    #[test]
    fn content_id_is_stable_and_alias_insensitive() {
        // Sandboxed and ReadOnly share a rank, so an exec=Sandboxed
        // grant and an exec=ReadOnly grant address identically.
        let g1 = Grant::new(Level::None, Level::None, Level::Sandboxed);
        let g2 = Grant::new(Level::None, Level::None, Level::ReadOnly);
        assert_eq!(g1.content_id(), g2.content_id());
        // Different authority -> different id.
        assert_ne!(Grant::bottom().content_id(), Grant::top().content_id());
        // Stable across calls.
        assert_eq!(g1.content_id(), g1.content_id());
        assert_eq!(g1.content_id().0.len(), 64);
    }
}
