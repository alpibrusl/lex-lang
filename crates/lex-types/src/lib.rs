//! M3: type system, effect system. See spec §6, §7.

pub mod types;
pub mod unifier;
pub mod env;
pub mod error;
pub mod builtins;
pub mod checker;

pub use checker::{check_program, ProgramTypes};
pub use error::TypeError;
pub use types::{EffectSet, Prim, Scheme, Ty, TyVarId};
