//! Effect-soundness of the type checker: a body can never hide an effect
//! it actually performs.
//!
//! This is the checker-level half of lex-lang#614 — the deeper companion
//! to the lattice properties in `trust_lattice.rs`. The whole sandbox
//! rests on one guarantee: *the declared effect row is an honest upper
//! bound on what the body does.* `checker.rs` enforces it by rejecting any
//! function whose inferred effects are not a subset of its declared row.
//! If that ever leaks — a body that touches `net` type-checking under a
//! row that omits `net` — the "rejected before a byte runs" promise is
//! void.
//!
//! These tests assert the property from the *outside*, exactly as the
//! `agent-tool` sandbox uses it: build a body that performs effect E,
//! then confirm the program is **rejected** whenever the declared row
//! omits E, and **accepted** when it includes E. Every rejection case is
//! paired with an identical-body acceptance case, so the body is provably
//! well-typed and the rejection can only be due to the effect — never a
//! vacuous pass on an unrelated type error.
//!
//! Coverage is deliberately *compositional*: the same primitive is buried
//! in a `let`, an `if`, a `match`, and behind a function call, because
//! effect-propagation bugs hide in the control-flow plumbing, not in the
//! direct call.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::check_program;

/// Type-check a whole program; `Ok(())` iff it type-checks.
fn checks(src: &str) -> bool {
    let Ok(p) = parse_source(src) else {
        return false;
    };
    let stages = canonicalize_program(&p);
    check_program(&stages).is_ok()
}

/// An effectful builtin reduced to the minimum needed to call it: the
/// import line, a well-typed call expression, the effect it performs, and
/// its return type as written in a signature.
struct Prim {
    import: &'static str,
    /// A call expression that performs exactly `effect` and has type `ret`.
    call: &'static str,
    effect: &'static str,
    ret: &'static str,
    /// Whether `ret` is `Int` — gates the arithmetic/`if` wrappers.
    int_ret: bool,
}

fn prims() -> Vec<Prim> {
    vec![
        Prim {
            import: "import \"std.time\" as time",
            call: "time.now()",
            effect: "time",
            ret: "Int",
            int_ret: true,
        },
        Prim {
            import: "import \"std.time\" as time",
            call: "time.now_ms()",
            effect: "time",
            ret: "Int",
            int_ret: true,
        },
        Prim {
            import: "import \"std.rand\" as rand",
            call: "rand.int_in(0, 1)",
            effect: "rand",
            ret: "Int",
            int_ret: true,
        },
        Prim {
            import: "import \"std.io\" as io",
            call: "io.print(\"x\")",
            effect: "io",
            ret: "Nil",
            int_ret: false,
        },
        Prim {
            import: "import \"std.env\" as env",
            call: "env.get(\"HOME\")",
            effect: "env",
            ret: "Option[Str]",
            int_ret: false,
        },
    ]
}

/// All the ways to embed `call` (of type `ret`) into a function body. Each
/// returns the body text for a function declared to return `ret`.
fn bodies(p: &Prim) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("direct", p.call.to_string()),
        ("let", format!("let x := {}\n  x", p.call)),
        ("match", format!("match 0 {{ _ => {} }}", p.call)),
    ];
    if p.int_ret {
        v.push(("if", format!("if 0 == 0 {{ {} }} else {{ 0 }}", p.call)));
    }
    v
}

/// A function declared to return `ret` with effect row `eff` (e.g. "time"
/// or "" for the empty row), wrapping `body`.
fn func(decl_effect: &str, ret: &str, body: &str) -> String {
    let row = if decl_effect.is_empty() {
        String::new()
    } else {
        format!("[{decl_effect}] ")
    };
    format!("fn f() -> {row}{ret} {{\n  {body}\n}}\n")
}

#[test]
fn declaring_the_effect_is_accepted() {
    // Sanity / non-vacuity anchor: every honest program (body declares the
    // effect it performs) type-checks. If any of these fail, the
    // corresponding rejection test below would be meaningless, so this
    // guards the whole file.
    for p in prims() {
        for (ctx, body) in bodies(&p) {
            let src = format!("{}\n{}", p.import, func(p.effect, p.ret, &body));
            assert!(
                checks(&src),
                "honest program should type-check ({} via {ctx}):\n{src}",
                p.effect
            );
        }
    }
}

#[test]
fn omitting_a_performed_effect_is_rejected() {
    // THE soundness property. Same bodies as above, but the declared row
    // omits the effect the body performs. Each must be rejected — the
    // effect must not escape the declaration, in any control-flow context.
    for p in prims() {
        for (ctx, body) in bodies(&p) {
            let src = format!("{}\n{}", p.import, func("", p.ret, &body));
            assert!(
                !checks(&src),
                "effect `{}` escaped an empty declared row via {ctx} — soundness hole:\n{src}",
                p.effect
            );
        }
    }
}

#[test]
fn effect_propagates_through_a_function_call() {
    // A caller that invokes an effectful helper inherits its effect. A
    // caller declaring a narrower row than the helper it calls must be
    // rejected — effects compose across application, the place propagation
    // bugs most often hide.
    for p in prims() {
        let helper = format!(
            "fn helper() -> [{}] {} {{\n  {}\n}}\n",
            p.effect, p.ret, p.call
        );
        // Honest caller: declares the effect it inherits.
        let honest = format!(
            "{}\n{}fn f() -> [{}] {} {{\n  helper()\n}}\n",
            p.import, helper, p.effect, p.ret
        );
        assert!(checks(&honest), "honest caller should check:\n{honest}");
        // Lying caller: calls the effectful helper but declares no effects.
        let lying = format!(
            "{}\n{}fn f() -> {} {{\n  helper()\n}}\n",
            p.import, helper, p.ret
        );
        assert!(
            !checks(&lying),
            "effect `{}` escaped through a call boundary — soundness hole:\n{lying}",
            p.effect
        );
    }
}

#[test]
fn every_member_of_a_multi_effect_body_must_be_declared() {
    // A body performing two distinct effects must declare *both*; dropping
    // either is rejected. This pins that the checker tracks the union of a
    // body's effects, not just "some effect happened".
    let import = "import \"std.time\" as time\nimport \"std.rand\" as rand";
    let body = "let a := time.now()\n  let b := rand.int_in(0, 1)\n  a + b";

    // Honest: declares both effects.
    assert!(
        checks(&format!("{import}\n{}", func("time, rand", "Int", body))),
        "declaring both effects should check"
    );
    // Dropping either one, or both, must be rejected.
    for missing in ["time", "rand", ""] {
        let src = format!("{import}\n{}", func(missing, "Int", body));
        assert!(
            !checks(&src),
            "a two-effect body type-checked under row `[{missing}]` — soundness hole:\n{src}"
        );
    }
}

/// The same Int-returning single-effect primitives the libfuzzer target
/// (`fuzz/fuzz_targets/effect_soundness.rs`) composes. Kept in sync so this
/// deterministic sweep is an always-on (stable-toolchain) guard for the
/// generator the fuzz job explores randomly under nightly.
const FUZZ_PRIMS: &[(&str, &str)] = &[
    ("time.now()", "time"),
    ("time.now_ms()", "time"),
    ("rand.int_in(0, 1)", "rand"),
];

fn wrap_int(kind: usize, expr: &str) -> String {
    match kind {
        0 => expr.to_string(),
        1 => format!("let x := {expr}\n  x"),
        2 => format!("if 0 == 0 {{ {expr} }} else {{ 0 }}"),
        _ => format!("match 0 {{ _ => {expr} }}"),
    }
}

#[test]
fn composition_sweep_is_sound() {
    // Exhaustively enumerate the libfuzzer generator's structural space —
    // 1..=3 composed primitives × 4 control-flow wrappers — and assert the
    // contract for every shape: the honest program (all effects declared)
    // checks, and every strict subset of the declared row is rejected.
    // Exhaustive on stable beats the random nightly fuzz for this finite
    // shape, and protects the fuzz target from emitting false crashes.
    let p = FUZZ_PRIMS;
    let mut shapes = 0u32;
    for n in 1..=3usize {
        // Every assignment of the n slots to a primitive index.
        let combos = p.len().pow(n as u32);
        for combo in 0..combos {
            // Decode `combo` into n primitive indices (base p.len()).
            let mut idx = combo;
            let mut calls = Vec::new();
            let (mut used_time, mut used_rand) = (false, false);
            for _ in 0..n {
                let (call, eff) = p[idx % p.len()];
                idx /= p.len();
                calls.push(call);
                match eff {
                    "time" => used_time = true,
                    _ => used_rand = true,
                }
            }
            let expr = calls.join(" + ");
            for wrapper in 0..4 {
                let body = wrap_int(wrapper, &expr);
                let mut imports = String::new();
                if used_time {
                    imports.push_str("import \"std.time\" as time\n");
                }
                if used_rand {
                    imports.push_str("import \"std.rand\" as rand\n");
                }
                let mut used: Vec<&str> = Vec::new();
                if used_time {
                    used.push("time");
                }
                if used_rand {
                    used.push("rand");
                }
                let honest = format!(
                    "{imports}fn f() -> [{}] Int {{\n  {body}\n}}\n",
                    used.join(", ")
                );
                assert!(checks(&honest), "honest sweep program failed:\n{honest}");

                // Every strict subset of the performed effects must be rejected.
                let subsets: Vec<&[&str]> = match used.as_slice() {
                    ["time", "rand"] => vec![&[], &["time"], &["rand"]],
                    _ => vec![&[]], // single effect: only the empty row is a strict subset
                };
                for sub in subsets {
                    let lying = format!("{imports}{}", func(&sub.join(", "), "Int", &body));
                    assert!(
                        !checks(&lying),
                        "effect escaped in sweep (performed {used:?}, declared {sub:?}):\n{lying}"
                    );
                }
                shapes += 1;
            }
        }
    }
    assert!(
        shapes >= 150,
        "expected to sweep the full shape space, got {shapes}"
    );
}

#[test]
fn a_pure_body_needs_no_effects() {
    // Control for false positives: a body that performs no effect checks
    // under the empty row. (Soundness is a one-sided guarantee — the row
    // must cover the body — so over-declaring is allowed and not tested
    // as a rejection.)
    assert!(checks("fn f() -> Int {\n  1 + 2\n}\n"));
}
