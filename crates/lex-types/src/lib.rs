//! M3: type system, effect system. See spec §6, §7.

#![allow(clippy::result_large_err)]

pub mod types;
pub mod unifier;
pub mod env;
pub mod error;
pub mod position;
pub mod rules;
pub mod builtins;
pub mod checker;
pub mod discharge;
pub mod trust;

pub use checker::{
    check_and_rewrite_program, check_program, check_program_with_positions, ProgramTypes,
};
pub use error::{PositionedError, TypeError};
pub use position::{byte_to_line_col, Position};
pub use rules::{all_rules, suggested_transform_for, RuleInfo};
pub use trust::{Dimension, Grant, GrantId, Level, TrustError};
pub use types::{EffectSet, Prim, Scheme, Ty, TyVarId};
