//! M3: type system, effect system. See spec §6, §7.

#![allow(clippy::result_large_err)]

pub mod types;
pub mod unifier;
pub mod env;
pub mod error;
pub mod builtins;
pub mod checker;

pub use checker::{check_and_rewrite_program, check_program, ProgramTypes};
pub use error::TypeError;
pub use types::{EffectSet, Prim, Scheme, Ty, TyVarId};
