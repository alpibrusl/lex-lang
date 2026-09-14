//! Store-backed [`lex_vcs::ResolutionChecker`] (#834).
//!
//! `lex-vcs`'s [`MergeSession`](lex_vcs::MergeSession) knows the *shape*
//! of a merge — which sig resolves to which stage — but can't compose a
//! program from stage ids: it has no `lex-store` dependency, by design.
//! This adapter closes that gap. Constructed with the store and the dst
//! branch of a merge in flight, it answers "does this resolution's
//! projected program type-check?" by delegating to
//! [`Store::typecheck_merge_projection`], which overlays the projected
//! delta on dst's head and runs the type checker without moving the
//! head.
//!
//! Wire it into `resolve_checked`:
//! ```ignore
//! let checker = MergeResolutionChecker::new(&store, dst_branch);
//! let verdicts = session.resolve_checked(pairs, &checker);
//! ```

use std::collections::BTreeMap;

use crate::store::Store;

/// Adapter turning a [`Store`] + dst branch into a
/// [`lex_vcs::ResolutionChecker`] for one merge session.
pub struct MergeResolutionChecker<'a> {
    store: &'a Store,
    dst_branch: String,
}

impl<'a> MergeResolutionChecker<'a> {
    /// `dst_branch` is the branch the merge lands on — the same branch
    /// whose head the projection is overlaid upon. It must not have
    /// moved since the session started (merges are held open in
    /// process memory; the branch head only advances at commit).
    pub fn new(store: &'a Store, dst_branch: impl Into<String>) -> Self {
        Self { store, dst_branch: dst_branch.into() }
    }
}

impl lex_vcs::ResolutionChecker for MergeResolutionChecker<'_> {
    fn typecheck_projection(&self, delta: &BTreeMap<String, Option<String>>) -> Vec<String> {
        match self.store.typecheck_merge_projection(&self.dst_branch, delta) {
            Ok(()) => Vec::new(),
            Err(crate::StoreError::TypeError(errors)) => {
                errors.iter().map(|e| e.to_string()).collect()
            }
            // A read/IO failure isn't a type error, but the session's
            // Vec<String> channel can't distinguish them — surface it as
            // a single diagnostic so the resolution is rejected loudly
            // rather than silently accepted.
            Err(other) => vec![format!("merge projection check failed: {other}")],
        }
    }
}
