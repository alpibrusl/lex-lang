//! Compiled program: a set of functions plus a constant pool.

use crate::op::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Program {
    pub constants: Vec<Const>,
    pub functions: Vec<Function>,
    /// Global function names → function index in `functions`.
    pub function_names: IndexMap<String, u32>,
    /// Entry function (for `lex run`, set to whatever function the user
    /// chose to invoke). Optional.
    pub entry: Option<u32>,
}

impl Program {
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.function_names.get(name).copied()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Function {
    pub name: String,
    pub arity: u16,
    pub locals_count: u16,
    pub code: Vec<Op>,
}
