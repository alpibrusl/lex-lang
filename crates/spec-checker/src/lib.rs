//! M10: Spec proof checker. Spec §14.
//!
//! Specs are properties attached to a function signature. The checker
//! tries to prove a spec holds for every input that satisfies the
//! quantifier constraints, and reports one of:
//!
//! - `proved` — the property holds (by randomized search across N trials,
//!   or by an SMT prover when one is available).
//! - `counterexample` — concrete inputs that falsify the property.
//! - `inconclusive` — the search couldn't decide one way or the other.
//!
//! ### Strategies
//!
//! - **Randomized** (default, pure-Rust). Generates inputs from a
//!   deterministic seed, checks the property by actually running the
//!   target function in the Lex VM. Reports `proved` after surviving
//!   1000 trials by default. This is honest about the method via
//!   `evidence.method = "randomized"`.
//! - **SMT-LIB export**. The spec can be lowered to SMT-LIB text for
//!   pasting into an external Z3 (`z3 -smt2 file.smt`). We don't link
//!   libz3 here to keep the dep surface light; that's a follow-up
//!   feature flag.
//!
//! ### Spec DSL
//!
//! ```text
//! spec clamp {
//!   forall x :: Int, lo :: Int, hi :: Int where lo <= hi:
//!     let r := clamp(x, lo, hi)
//!     (r >= lo) and (r <= hi)
//! }
//! ```

mod ast;
mod parser;
mod checker;
mod smt;

pub use ast::{Quantifier, Spec, SpecExpr, SpecOp, SpecType};
pub use checker::{check_spec, CheckResult, Evidence, ProofStatus};
pub use parser::{parse_spec, SpecParseError};
pub use smt::to_smtlib;
