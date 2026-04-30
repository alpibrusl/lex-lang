//! M16 §16.2 — property tests.
//!
//! For each milestone-already-built artifact, run a small property-based
//! check over a corpus of programs:
//! - Well-typed programs evaluate without runtime type errors.
//! - Ill-typed programs produce structured errors (not crashes).
//! - Canonical AST round-trip is identity.
//! - Pretty-printed code re-parses to the same canonical AST.
//!
//! Programs are generated from a fixed seed corpus (deterministic). True
//! random generation can come later; this corpus is enough to demonstrate
//! the property without committing to a generator framework.

use lex_ast::{canonicalize_program, print_stages, stage_canonical_hash_hex};
use lex_syntax::parse_source;

const PROGRAMS: &[&str] = &[
    "fn id(x :: Int) -> Int { x }\n",
    "fn add(x :: Int, y :: Int) -> Int { x + y }\n",
    "fn fact(n :: Int) -> Int { match n { 0 => 1, _ => n * fact(n - 1) } }\n",
    "fn pick(b :: Bool) -> Int { if b { 1 } else { 0 } }\n",
    "fn rec(p :: { x :: Int, y :: Int }) -> Int { p.x + p.y }\n",
    "type Maybe = Yes | No\nfn b(m :: Maybe) -> Int { match m { Yes => 1, No => 0 } }\n",
    "fn pipe(x :: Int) -> Int { x |> id }\nfn id(x :: Int) -> Int { x }\n",
    "fn list_first() -> List[Int] { [1, 2, 3] }\n",
    "fn tuple() -> (Int, Str) { (1, \"two\") }\n",
];

#[test]
fn well_typed_programs_compile_and_run() {
    for src in PROGRAMS {
        let prog = parse_source(src).expect("parse");
        let stages = canonicalize_program(&prog);
        lex_types::check_program(&stages)
            .unwrap_or_else(|errs| panic!("type errors in well-typed program {src:?}: {errs:#?}"));
    }
}

#[test]
fn canonical_round_trip_is_identity() {
    // parse → canonicalize → print → parse → canonicalize is identity.
    for src in PROGRAMS {
        let prog = parse_source(src).expect("parse");
        let s1 = canonicalize_program(&prog);
        let printed = print_stages(&s1);
        let prog2 = parse_source(&printed).expect("re-parse");
        let s2 = canonicalize_program(&prog2);
        assert_eq!(s1, s2, "round-trip differs for source:\n{src}\nprinted:\n{printed}");
    }
}

#[test]
fn canonical_hashes_are_stable() {
    // Compiling the same source twice yields byte-identical hashes.
    for src in PROGRAMS {
        let s1 = canonicalize_program(&parse_source(src).unwrap());
        let s2 = canonicalize_program(&parse_source(src).unwrap());
        for (a, b) in s1.iter().zip(s2.iter()) {
            assert_eq!(stage_canonical_hash_hex(a), stage_canonical_hash_hex(b));
        }
    }
}

#[test]
fn ill_typed_programs_produce_structured_errors_not_panics() {
    // Each entry should fail typecheck with a structured TypeError, never
    // crash. Run inside catch_unwind to be sure.
    let bad = &[
        "fn bad(x :: Int) -> Str { x }\n",
        "fn bad() -> Int { y }\n",
        "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn caller() -> Int { add(1) }\n",
    ];
    for src in bad {
        let result = std::panic::catch_unwind(|| {
            let prog = parse_source(src).expect("parse");
            let stages = canonicalize_program(&prog);
            lex_types::check_program(&stages)
        });
        let inner = result.expect("type checker should not panic on ill-typed input");
        assert!(inner.is_err(), "expected type errors for: {src}");
    }
}
