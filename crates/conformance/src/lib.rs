//! M16: conformance harness. Spec §16.
//!
//! Reads JSON test descriptors and runs each through the full pipeline:
//! parse → typecheck → compile → policy gate → execute. Compares the
//! observed status and output against the descriptor's expectations.
//!
//! The descriptor schema (§16.1):
//!
//! ```json
//! {
//!   "name": "factorial",
//!   "language": "lex",
//!   "source": "...",
//!   "fn": "factorial",
//!   "input": [5],
//!   "expected_output": 120,
//!   "policy": { "allow_effects": [] },
//!   "expected_status": "ok" | { "error_kind": "..." }
//! }
//! ```

mod descriptor;
mod runner;
mod tokens;

pub use descriptor::{Descriptor, ExpectedStatus, PolicyJson};
pub use runner::{run_descriptor, run_directory, Outcome, Report};
pub use tokens::{count_tokens, GRAMMAR_REFERENCE};
