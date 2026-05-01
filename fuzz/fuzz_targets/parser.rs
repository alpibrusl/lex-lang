//! Fuzz the parser. Goal: find inputs that crash `parse_source` —
//! panics, OOMs, or unbounded recursion. Parse errors are *not*
//! crashes; the parser is allowed to refuse anything it likes, it
//! just shouldn't unwind or deadlock.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = lex_syntax::parse_source(s);
    }
});
