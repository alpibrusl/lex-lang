//! Fuzz the type checker. Feeds parser-accepted programs into
//! `check_program` and asserts the checker doesn't panic. Type
//! errors are fine; crashes (panic, stack overflow, OOM) are not.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let Ok(prog) = lex_syntax::parse_source(s) else { return };
    let stages = lex_ast::canonicalize_program(&prog);
    let _ = lex_types::check_program(&stages);
});
