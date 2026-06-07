//! Fuzz the checker's effect-soundness guarantee: a body can never hide an
//! effect it performs.
//!
//! Unlike `type_checker.rs` (which only asserts the checker doesn't panic
//! on arbitrary input), this target builds *well-typed* programs from a
//! fixed menu of effectful primitives, composes them through random
//! control flow, and checks the safety contract directly:
//!
//!   * the **honest** program — declaring every effect the body performs —
//!     must type-check (otherwise the generator emitted something
//!     ill-typed, which is a bug in this target, surfaced as a panic); and
//!   * every **lying** program — same body, but a declared row missing at
//!     least one performed effect — must be **rejected**. A lying program
//!     that type-checks is an effect escaping its declaration: a soundness
//!     hole, surfaced as a panic with the offending source.
//!
//! The libFuzzer input is consumed purely as a stream of structural
//! choices, so the corpus explores composition shapes (which primitives,
//! how they nest) rather than raw token soup.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// An Int-returning, single-effect primitive: (call expression, effect).
const PRIMS: &[(&str, &str)] = &[
    ("time.now()", "time"),
    ("time.now_ms()", "time"),
    ("rand.int_in(0, 1)", "rand"),
];

/// A tiny byte-driven chooser so the fuzzer's bytes map to structural
/// decisions deterministically.
struct Choices<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Choices<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Choices { bytes, pos: 0 }
    }
    /// Next byte (0 when exhausted — keeps generation total).
    fn next(&mut self) -> u8 {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn pick(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

/// Type-check a program; true iff it type-checks.
fn checks(src: &str) -> bool {
    let Ok(p) = lex_syntax::parse_source(src) else {
        return false;
    };
    let stages = lex_ast::canonicalize_program(&p);
    lex_types::check_program(&stages).is_ok()
}

/// Wrap an Int-typed expression in one of several control-flow contexts,
/// each of which preserves both its type (Int) and its effects.
fn wrap(c: &mut Choices, expr: &str) -> String {
    match c.pick(4) {
        0 => expr.to_string(),
        1 => format!("let x := {expr}\n  x"),
        2 => format!("if 0 == 0 {{ {expr} }} else {{ 0 }}"),
        _ => format!("match 0 {{ _ => {expr} }}"),
    }
}

/// A declared effect row from a set of effect names; empty set → no row.
fn row(effects: &[&str]) -> String {
    if effects.is_empty() {
        String::new()
    } else {
        format!("[{}] ", effects.join(", "))
    }
}

fuzz_target!(|data: &[u8]| {
    let mut c = Choices::new(data);

    // Choose 1..=3 primitive calls and compose them with `+` (all Int).
    let n = 1 + c.pick(3);
    let mut calls = Vec::new();
    let mut used_time = false;
    let mut used_rand = false;
    for _ in 0..n {
        let (call, eff) = PRIMS[c.pick(PRIMS.len())];
        calls.push(call);
        match eff {
            "time" => used_time = true,
            "rand" => used_rand = true,
            _ => unreachable!(),
        }
    }
    let expr = calls.join(" + ");
    let body = wrap(&mut c, &expr);

    // Imports: exactly the modules used (an unused import would be its own
    // diagnostic and muddy the contract).
    let mut imports = String::new();
    if used_time {
        imports.push_str("import \"std.time\" as time\n");
    }
    if used_rand {
        imports.push_str("import \"std.rand\" as rand\n");
    }

    // The set of effects the body actually performs.
    let mut used: Vec<&str> = Vec::new();
    if used_time {
        used.push("time");
    }
    if used_rand {
        used.push("rand");
    }

    // Honest program: declare every performed effect. Must type-check —
    // if it doesn't, this generator emitted an ill-typed body (a bug here,
    // not in the checker).
    let honest = format!("{imports}fn f() -> {}Int {{\n  {body}\n}}\n", row(&used));
    assert!(
        checks(&honest),
        "generator emitted an ill-typed honest program:\n{honest}"
    );

    // Lying programs: every *strict* subset of the performed effects (drop
    // at least one). Each must be rejected — the dropped effect must not
    // escape.
    let subsets: &[&[&str]] = match used.as_slice() {
        ["time", "rand"] => &[&[], &["time"], &["rand"]],
        ["time"] => &[&[]],
        ["rand"] => &[&[]],
        _ => &[],
    };
    for missing_row in subsets {
        let lying = format!(
            "{imports}fn f() -> {}Int {{\n  {body}\n}}\n",
            row(missing_row)
        );
        assert!(
            !checks(&lying),
            "an effect escaped its declaration — soundness hole.\nperformed: {used:?}\ndeclared: {missing_row:?}\n{lying}"
        );
    }
});
