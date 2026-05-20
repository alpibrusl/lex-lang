//! M4: bytecode definition, compiler, VM (pure subset). See spec §8.

pub mod op;
pub mod program;
pub mod value;
pub mod compiler;
pub mod conc_registry;
pub mod parser_runtime;
pub mod vm;
pub mod verify;

pub use compiler::compile_program;
pub use op::{Const, Op};
pub use program::{Function, Program};
pub use value::{MapKey, Value};
pub use vm::{Vm, VmError, MAX_CALL_DEPTH};
pub use verify::{verify_program, StackError};
