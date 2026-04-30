//! M4: bytecode definition, compiler, VM (pure subset). See spec §8.

pub mod op;
pub mod program;
pub mod value;
pub mod compiler;
pub mod vm;

pub use compiler::compile_program;
pub use op::{Const, Op};
pub use program::{Function, Program};
pub use value::Value;
pub use vm::{Vm, VmError};
