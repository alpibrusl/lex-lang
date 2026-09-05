//! Runtime side of the declarative builtin catalogue (#778).
//!
//! `lex_types::stdlib_spec` declares each builtin once (signature, purity,
//! docs); this module holds the implementations for the declared modules,
//! keyed by the same `(module, name)`, and `lookup` is what the pure
//! dispatch path consults before the legacy `match` in `builtins.rs`.
//! A test in `tests/stdlib_table_778.rs` asserts the two sets agree:
//! every `Pure` definition has exactly one implementation here, and
//! nothing here lacks a definition.
//!
//! Implementations take their arguments by value. The VM already owns
//! the argument vector on the hot path (`call_pure_builtin`), so a
//! builtin that returns a modified list moves it instead of cloning
//! (`list.cons`, `list.tail`); the borrowed entry point clones once
//! before calling in.

use lex_bytecode::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

pub(crate) mod list;
pub(crate) mod str;

/// Signature every declared pure builtin implements.
pub(crate) type BuiltinFn = fn(Vec<Value>) -> Result<Value, String>;

/// One row of a module's implementation table.
pub(crate) type Entry = (&'static str, BuiltinFn);

/// Every implemented builtin as `((module, name), fn)`.
pub(crate) fn entries() -> Vec<((&'static str, &'static str), BuiltinFn)> {
    let mut out = Vec::new();
    for (module, table) in [("str", str::TABLE), ("list", list::TABLE)] {
        for (name, f) in table {
            out.push(((module, *name), *f));
        }
    }
    out
}

pub(crate) fn lookup(module: &str, name: &str) -> Option<BuiltinFn> {
    static TABLE: OnceLock<HashMap<(&'static str, &'static str), BuiltinFn>> = OnceLock::new();
    let table = TABLE.get_or_init(|| entries().into_iter().collect());
    table.get(&(module, name)).copied()
}
