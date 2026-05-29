//! M4: bytecode definition, compiler, VM (pure subset). See spec §8.

pub mod op;
pub mod program;
pub mod value;
pub mod shape_registry;
pub mod compiler;
pub mod conc_registry;
pub mod parser_runtime;
pub mod vm;
pub mod verify;
pub mod escape;
pub mod arena;

pub use compiler::compile_program;
pub use op::{Const, Op};
pub use program::{Function, Program};
pub use value::{MapKey, Value};
pub use vm::{Vm, VmError, MAX_CALL_DEPTH};
pub use verify::{verify_program, StackError};
pub use escape::{analyze_program as analyze_escapes, EscapeReport, EscapeSite, Policy, SiteKind};
pub use arena::{
    analyze_program as analyze_arena, build_arena_index, ArenaReport, ArenaSite,
};
