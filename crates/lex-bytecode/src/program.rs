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
    /// Imported module aliases → module name (e.g., `io` → `io`).
    /// Used by the compiler/runtime to dispatch `alias.op(...)` calls.
    pub module_aliases: IndexMap<String, String>,
    /// Entry function (for `lex run`, set to whatever function the user
    /// chose to invoke). Optional.
    pub entry: Option<u32>,
}

impl Program {
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.function_names.get(name).copied()
    }

    /// Walk every function's declared effects and collect the union of
    /// effect kinds (with their args).
    pub fn declared_effects(&self) -> Vec<DeclaredEffect> {
        let mut out: Vec<DeclaredEffect> = Vec::new();
        for f in &self.functions {
            for e in &f.effects {
                if !out.iter().any(|x| x == e) {
                    out.push(e.clone());
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Function {
    pub name: String,
    pub arity: u16,
    pub locals_count: u16,
    pub code: Vec<Op>,
    /// Declared effects on this function's signature (spec §7).
    #[serde(default)]
    pub effects: Vec<DeclaredEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeclaredEffect {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<EffectArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EffectArg {
    Str(String),
    Int(i64),
    Ident(String),
}
