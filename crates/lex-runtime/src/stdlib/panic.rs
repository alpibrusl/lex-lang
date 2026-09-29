//! `std.panic` — one builtin (#778-style: catalogue in
//! `lex_types::stdlib_spec`, implementation here).
//!
//! `todo` is effect-free by design: see the doc comment on its
//! `BuiltinDef` for why a stub marker should carry no effect of its own.

use super::Entry;
use crate::builtins::expect_str;
use lex_bytecode::Value;

pub(crate) const TABLE: &[Entry] = &[("todo", todo)];

fn todo(args: Vec<Value>) -> Result<Value, String> {
    Err(format!("not yet implemented: {}", expect_str(args.first())?))
}
