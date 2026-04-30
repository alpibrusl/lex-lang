//! M9 (Phase 1): Core, the performance sibling. Spec §13.
//!
//! This Phase 1 ships the type-system pieces of Core that don't depend on
//! native codegen:
//! - **Sized numeric types** (U8..U64, I8..I64, F32/F64) are recognized
//!   alongside Lex's `Int`/`Float`. They share runtime representation
//!   for now (both are 64-bit) but have distinct identities for type
//!   checking.
//! - **Tensor shape language** with a shape solver: tensor types are
//!   parameterized by a list of `ShapeExpr`s (nat literals, type
//!   variables, or arithmetic over them). `matmul` and friends check at
//!   compile time that inner dimensions agree.
//! - **`check_core_stage`**: runs Lex's HM type-checker first, then a
//!   Core extras pass (shape solver, future mutation analysis).
//!
//! Native Cranelift codegen, mutation analysis, `for` loops, packed
//! structs, and `arena` blocks land in Phase 2.

pub mod shape;
pub mod check;
pub mod error;
pub mod mutation;
pub mod native;

pub use check::{check_core_stage, CoreStage};
pub use error::CoreError;
pub use mutation::{check_no_mut_return, CoreExpr};
pub use native::{make_matrix, NativeFn, NativeRegistry};
pub use shape::{ShapeExpr, ShapeSolver, Tensor};
