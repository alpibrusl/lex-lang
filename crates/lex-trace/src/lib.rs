//! M7: trace tree and replay. Spec §10.
//!
//! At runtime the VM emits Call/Effect enter/exit events through the
//! `Tracer` trait. The `Recorder` here builds a tree of `TraceNode`s
//! keyed by AST `NodeId` (from the original canonical AST).
//!
//! Persistence: a trace is one JSON file per run. Replay re-executes the
//! program with overrides keyed by `NodeId`.

mod recorder;
mod replay;
mod diff;

pub use diff::{diff_runs, Divergence};
pub use recorder::{Recorder, RunId, TraceNode, TraceNodeKind, TraceTree};
pub use replay::{replay_with_overrides, Override};
