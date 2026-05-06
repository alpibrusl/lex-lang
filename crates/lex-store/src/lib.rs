//! M6: content-addressed store. Spec §4.
//!
//! Filesystem layout (§4.2):
//!
//! ```text
//! <root>/
//! ├── stages/
//! │   └── <SigId>/
//! │       ├── implementations/
//! │       │   ├── <StageId>.ast.json
//! │       │   └── <StageId>.metadata.json
//! │       ├── tests/
//! │       │   └── <test_id>.json
//! │       ├── specs/
//! │       │   └── <spec_id>.json
//! │       └── lifecycle.json
//! └── traces/
//!     └── <run_id>/
//!         └── trace.json
//! ```
//!
//! The filesystem is canonical. We don't ship `index.db` (the spec calls
//! it a cache); the in-memory index is rebuilt on `Store::open` by
//! scanning the filesystem. Acceptance §4.6 requires that this rebuild
//! produces identical results regardless of cache state — the obvious way
//! to honor that is to keep the cache trivially derivable.

mod store;
mod model;
mod branches;
pub mod users;

pub use lex_vcs::{OpId, Operation, OperationKind, OperationRecord, StageTransition};
pub use model::{Lifecycle, Metadata, Spec, StageStatus, Test, Transition};
pub use store::{PublishOp, PublishOutcome, StageHistoryEntry, Store, StoreError};
pub use branches::{
    Branch, MergeConflict, MergeEntry, MergeRecord, MergeReport, MergeSummary,
    DEFAULT_BRANCH,
};
