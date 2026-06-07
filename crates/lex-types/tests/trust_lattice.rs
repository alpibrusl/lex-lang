//! Exhaustive lattice & soundness properties for the trust model.
//!
//! The trust lattice is the type-level half of the two-enforcement-layer
//! story (lex-lang#614): the same [`Grant`] that the supervisor turns
//! into an OS sandbox is what the type checker uses to reject a program
//! calling an un-granted effect. The unit tests in `trust.rs` pin
//! specific cases; this file proves the *properties* the safety argument
//! rests on, and does so **exhaustively** — the canonical grant space is
//! finite (7 `Level` variants over 3 dimensions = 343 grants), so we
//! enumerate every grant and every pair rather than sampling.
//!
//! Properties:
//!  1. `leq` is a partial order (reflexive, transitive, antisymmetric up
//!     to rank) — i.e. narrowing is a well-defined ⊑ relation.
//!  2. `narrow` accepts exactly the narrowing pairs: `narrow(p,c)` succeeds
//!     iff `c ⊑ p`. The static narrowing wall has no gaps and no false
//!     refusals.
//!  3. **Soundness under narrowing.** If a narrower grant permits an effect
//!     set, every wider grant does too — equivalently, *narrowing a grant
//!     never makes a previously-rejected effect set type-check*. This is
//!     the grant-level statement of effect soundness.
//!  4. `join`/`meet` are the lattice lub/glb.
//!  5. `permits_effect` agrees, effect-by-effect, with the published
//!     `effect_requirement` table — the bridge from effect names to the
//!     lattice has no drift.

use lex_types::trust::{effect_requirement, Dimension, Grant, Level};
use lex_types::types::{EffectKind, EffectSet};

/// All `Level` variants, including the rank-aliases.
const ALL_LEVELS: [Level; 7] = [
    Level::None,
    Level::ReadOnly,
    Level::Sandboxed,
    Level::Loopback,
    Level::ReadWrite,
    Level::Allowlist,
    Level::Full,
];

/// One representative `Level` per rank (0..=3) — enough to enumerate the
/// lattice shape without the alias blow-up, for the cubic property.
const RANK_REPS: [Level; 4] = [Level::None, Level::ReadOnly, Level::ReadWrite, Level::Full];

fn all_grants() -> Vec<Grant> {
    let mut g = Vec::with_capacity(343);
    for &fs in &ALL_LEVELS {
        for &net in &ALL_LEVELS {
            for &exec in &ALL_LEVELS {
                g.push(Grant::new(fs, net, exec));
            }
        }
    }
    g
}

fn rank_rep_grants() -> Vec<Grant> {
    let mut g = Vec::with_capacity(64);
    for &fs in &RANK_REPS {
        for &net in &RANK_REPS {
            for &exec in &RANK_REPS {
                g.push(Grant::new(fs, net, exec));
            }
        }
    }
    g
}

/// A spread of effect sets covering every entry in the trust vocabulary
/// (host-scoped and bare net, every dimension) plus pure effects and a
/// combined set — the inputs the soundness property quantifies over.
fn sample_effect_sets() -> Vec<EffectSet> {
    let singles = [
        EffectKind::bare("fs_read"),
        EffectKind::bare("fs_walk"),
        EffectKind::bare("fs_write"),
        EffectKind::bare("net"),
        EffectKind::with_str("net", "results.demo.internal"),
        EffectKind::bare("http"),
        EffectKind::bare("mcp"),
        EffectKind::bare("llm_cloud"),
        EffectKind::bare("proc"),
        EffectKind::bare("llm_local"),
        EffectKind::bare("log"),  // pure — always permitted
        EffectKind::bare("time"), // pure — always permitted
    ];
    let mut sets: Vec<EffectSet> = singles
        .iter()
        .cloned()
        .map(|e| {
            let mut s = EffectSet::empty();
            s.concrete.insert(e);
            s
        })
        .collect();

    // The empty set (does nothing — satisfies any grant).
    sets.push(EffectSet::empty());

    // A combined set that touches all three dimensions at once.
    let mut all_dims = EffectSet::empty();
    all_dims.concrete.insert(EffectKind::bare("fs_write"));
    all_dims.concrete.insert(EffectKind::bare("net"));
    all_dims.concrete.insert(EffectKind::bare("proc"));
    sets.push(all_dims);

    sets
}

#[test]
fn leq_is_reflexive() {
    for g in all_grants() {
        assert!(g.leq(&g), "leq not reflexive at {g}");
    }
}

#[test]
fn leq_is_transitive_and_antisymmetric() {
    // Cubic property over the rank-representative subset (leq depends only
    // on rank, so this loses no generality): 64³ ≈ 262k triples.
    let grants = rank_rep_grants();
    for a in &grants {
        for b in &grants {
            // Antisymmetry up to rank: mutual ⊑ means identical authority,
            // hence an identical content address.
            if a.leq(b) && b.leq(a) {
                assert_eq!(
                    a.content_id(),
                    b.content_id(),
                    "mutual leq but different content id: {a} vs {b}"
                );
            }
            if !a.leq(b) {
                continue;
            }
            for c in &grants {
                if b.leq(c) {
                    assert!(a.leq(c), "leq not transitive: {a} ⊑ {b} ⊑ {c}");
                }
            }
        }
    }
}

#[test]
fn narrow_accepts_exactly_the_narrowing_pairs() {
    // The static narrowing wall: narrow(parent, child) succeeds iff the
    // child narrows the parent. No gaps (a widening that slips through)
    // and no false refusals (a legitimate narrowing rejected).
    for parent in &all_grants() {
        for child in &all_grants() {
            let narrowed = Grant::narrow(parent, child);
            assert_eq!(
                narrowed.is_ok(),
                child.leq(parent),
                "narrow disagrees with leq for parent {parent}, child {child}"
            );
            if let Ok(g) = narrowed {
                assert_eq!(g, *child, "narrow must return the (validated) child");
            }
        }
    }
}

#[test]
fn narrowing_never_unlocks_a_rejected_effect_set() {
    // Effect soundness at the grant level: if the child (narrower) grant
    // permits an effect set, the parent (wider) grant must too. The
    // contrapositive is the property that matters operationally —
    // narrowing a grant can only ever *remove* permitted effects, never
    // make a previously-rejected one pass.
    let grants = all_grants();
    let effect_sets = sample_effect_sets();
    let mut pairs_checked = 0u64;
    for parent in &grants {
        for child in &grants {
            if !child.leq(parent) {
                continue;
            }
            pairs_checked += 1;
            for es in &effect_sets {
                if child.permits_effects(es).is_ok() {
                    assert!(
                        parent.permits_effects(es).is_ok(),
                        "child {child} permits an effect set the wider parent {parent} rejects"
                    );
                }
            }
        }
    }
    assert!(pairs_checked > 1000, "only {pairs_checked} narrowing pairs");
}

#[test]
fn join_and_meet_are_lub_and_glb() {
    for a in &all_grants() {
        for b in &all_grants() {
            let j = a.join(b);
            let m = a.meet(b);
            // Upper / lower bounds.
            assert!(a.leq(&j) && b.leq(&j), "join not an upper bound of {a},{b}");
            assert!(m.leq(a) && m.leq(b), "meet not a lower bound of {a},{b}");
            // Least / greatest: any common upper bound dominates the join,
            // any common lower bound is dominated by the meet. Check the
            // dimensionwise extreme witnesses suffice via rank equality.
            for &d in &Dimension::ALL {
                let jr = j.level(d).rank();
                let mr = m.level(d).rank();
                assert_eq!(jr, a.level(d).rank().max(b.level(d).rank()));
                assert_eq!(mr, a.level(d).rank().min(b.level(d).rank()));
            }
        }
    }
}

#[test]
fn permits_effect_agrees_with_the_requirement_table() {
    // The bridge from effect name -> (dimension, required level) is the
    // single source of truth; permits_effect must be exactly its pointwise
    // application, for every grant and every vocabulary entry.
    let vocab = [
        "fs_read",
        "fs_walk",
        "fs_write",
        "net",
        "http",
        "mcp",
        "llm_cloud",
        "proc",
        "llm_local",
        // pure / outside the model — always permitted:
        "log",
        "time",
        "rand",
        "crypto",
        "sql",
    ];
    for g in all_grants() {
        for name in vocab {
            let e = EffectKind::bare(name);
            let expected = match effect_requirement(name) {
                Some((dim, required)) => required.leq(g.level(dim)),
                None => true,
            };
            assert_eq!(
                g.permits_effect(&e),
                expected,
                "permits_effect disagrees with effect_requirement for `{name}` under {g}"
            );
        }
    }
}
