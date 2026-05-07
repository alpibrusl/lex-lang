//! Built-in module signatures used by §3.13 examples and beyond.
//!
//! These are stub signatures that let the type-checker verify code that
//! imports `std.io`, `std.str`, `std.list`, etc. They will be backed by
//! real stages once the stdlib lands (M11).

use crate::env::TypeEnv;
use crate::types::*;
use indexmap::IndexMap;

/// Build the value-level scope of a module: a record of named functions.
pub fn module_scope(name: &str, _env: &TypeEnv) -> Option<Ty> {
    match name {
        "io" => {
            let mut fields = IndexMap::new();
            // io.print(line :: Str) -> [io] Nil
            fields.insert("print".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("io"),
                Ty::Unit,
            ));
            // io.read(path :: Str) -> [io] Result[Str, Str]
            fields.insert("read".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("io"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            // io.write(path :: Str, contents :: Str) -> [io] Result[Unit, Str]
            fields.insert("write".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::singleton("io"),
                Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        "str" => {
            let mut fields = IndexMap::new();
            fields.insert("is_empty".into(), Ty::function(vec![Ty::str()], EffectSet::empty(), Ty::bool()));
            fields.insert("to_int".into(), Ty::function(vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::int()])));
            fields.insert("to_float".into(), Ty::function(vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::float()])));
            fields.insert("concat".into(), Ty::function(vec![Ty::str(), Ty::str()], EffectSet::empty(), Ty::str()));
            fields.insert("len".into(), Ty::function(vec![Ty::str()], EffectSet::empty(), Ty::int()));
            fields.insert("split".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::str())),
            ));
            fields.insert("join".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::str())), Ty::str()],
                EffectSet::empty(),
                Ty::str(),
            ));
            // -- predicates --
            for name in &["starts_with", "ends_with", "contains"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::str(), Ty::str()],
                    EffectSet::empty(),
                    Ty::bool(),
                ));
            }
            // -- transformers --
            fields.insert("replace".into(), Ty::function(
                vec![Ty::str(), Ty::str(), Ty::str()],
                EffectSet::empty(),
                Ty::str(),
            ));
            for name in &["trim", "to_upper", "to_lower"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::str()], EffectSet::empty(), Ty::str(),
                ));
            }
            for name in &["strip_prefix", "strip_suffix"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::str(), Ty::str()],
                    EffectSet::empty(),
                    Ty::Con("Option".into(), vec![Ty::str()]),
                ));
            }
            // slice :: (Str, Int, Int) -> Str  — byte-range half-open
            fields.insert("slice".into(), Ty::function(
                vec![Ty::str(), Ty::int(), Ty::int()],
                EffectSet::empty(),
                Ty::str(),
            ));
            Some(Ty::Record(fields))
        }
        "int" => {
            let mut fields = IndexMap::new();
            fields.insert("to_str".into(), Ty::function(vec![Ty::int()], EffectSet::empty(), Ty::str()));
            fields.insert("to_float".into(), Ty::function(vec![Ty::int()], EffectSet::empty(), Ty::float()));
            Some(Ty::Record(fields))
        }
        "math" => {
            let mut fields = IndexMap::new();
            // Matrix is registered as a built-in type alias in
            // TypeEnv::new_with_builtins; refer to it nominally so call
            // sites unify against the user's `:: Matrix` annotations.
            let mat = || Ty::Con("Matrix".into(), Vec::new());
            // Scalar floats — single-arg `Float -> Float`.
            for name in &[
                "exp", "log", "log2", "log10", "sqrt", "abs",
                "sin", "cos", "tan", "asin", "acos", "atan",
                "floor", "ceil", "round", "trunc",
            ] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::float()], EffectSet::empty(), Ty::float(),
                ));
            }
            // Two-arg `Float, Float -> Float`.
            for name in &["pow", "atan2", "min", "max"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::float(), Ty::float()], EffectSet::empty(), Ty::float(),
                ));
            }
            // Constructors.
            fields.insert("zeros".into(), Ty::function(
                vec![Ty::int(), Ty::int()], EffectSet::empty(), mat(),
            ));
            fields.insert("ones".into(), Ty::function(
                vec![Ty::int(), Ty::int()], EffectSet::empty(), mat(),
            ));
            fields.insert("from_lists".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::List(Box::new(Ty::float()))))],
                EffectSet::empty(),
                mat(),
            ));
            fields.insert("from_flat".into(), Ty::function(
                vec![Ty::int(), Ty::int(), Ty::List(Box::new(Ty::float()))],
                EffectSet::empty(),
                mat(),
            ));
            // Accessors.
            fields.insert("rows".into(), Ty::function(vec![mat()], EffectSet::empty(), Ty::int()));
            fields.insert("cols".into(), Ty::function(vec![mat()], EffectSet::empty(), Ty::int()));
            fields.insert("get".into(), Ty::function(
                vec![mat(), Ty::int(), Ty::int()], EffectSet::empty(), Ty::float(),
            ));
            fields.insert("to_flat".into(), Ty::function(
                vec![mat()], EffectSet::empty(),
                Ty::List(Box::new(Ty::float())),
            ));
            // Linalg ops.
            fields.insert("transpose".into(), Ty::function(
                vec![mat()], EffectSet::empty(), mat(),
            ));
            fields.insert("matmul".into(), Ty::function(
                vec![mat(), mat()], EffectSet::empty(), mat(),
            ));
            fields.insert("scale".into(), Ty::function(
                vec![Ty::float(), mat()], EffectSet::empty(), mat(),
            ));
            for name in &["add", "sub"] {
                fields.insert((*name).into(), Ty::function(
                    vec![mat(), mat()], EffectSet::empty(), mat(),
                ));
            }
            fields.insert("sigmoid".into(), Ty::function(
                vec![mat()], EffectSet::empty(), mat(),
            ));
            Some(Ty::Record(fields))
        }
        "float" => {
            let mut fields = IndexMap::new();
            fields.insert("to_int".into(), Ty::function(vec![Ty::float()], EffectSet::empty(), Ty::int()));
            fields.insert("to_str".into(), Ty::function(vec![Ty::float()], EffectSet::empty(), Ty::str()));
            Some(Ty::Record(fields))
        }
        "list" => {
            // list polymorphic functions need fresh vars at use sites; we
            // encode them with placeholder Var ids that get instantiated.
            let mut fields = IndexMap::new();
            // Effect polymorphism: each HOF carries an effect-row
            // variable so an effectful closure (e.g. one that calls
            // net.get inside list.map's lambda) propagates its
            // effects to the result type. Spec §7.3.
            //
            // map :: [E] List[a], (a) -> [E] b -> [E] List[b]
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(2), Ty::Var(1)),
                ],
                EffectSet::open_var(2),
                Ty::List(Box::new(Ty::Var(1))),
            ));
            fields.insert("filter".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3), Ty::bool()),
                ],
                EffectSet::open_var(3),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            fields.insert("fold".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::Var(1),
                    Ty::function(vec![Ty::Var(1), Ty::Var(0)], EffectSet::open_var(4), Ty::Var(1)),
                ],
                EffectSet::open_var(4),
                Ty::Var(1),
            ));
            fields.insert("len".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::int(),
            ));
            fields.insert("is_empty".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::bool(),
            ));
            fields.insert("range".into(), Ty::function(
                vec![Ty::int(), Ty::int()],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::int())),
            ));
            fields.insert("head".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::Var(0)]),
            ));
            fields.insert("tail".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            fields.insert("concat".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0))), Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            Some(Ty::Record(fields))
        }
        "bytes" => {
            let mut fields = IndexMap::new();
            fields.insert("len".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::int(),
            ));
            fields.insert("is_empty".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::bool(),
            ));
            fields.insert("eq".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()], EffectSet::empty(), Ty::bool(),
            ));
            fields.insert("from_str".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(), Ty::bytes(),
            ));
            fields.insert("to_str".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            fields.insert("slice".into(), Ty::function(
                vec![Ty::bytes(), Ty::int(), Ty::int()],
                EffectSet::empty(), Ty::bytes(),
            ));
            Some(Ty::Record(fields))
        }
        "time" => {
            // time.now() -> [time] Int — unix timestamp seconds.
            // Reading the clock is an effect for two reasons: it's
            // non-deterministic (replay needs the captured value) and
            // it's a side-channel surface (see "Capability ≠
            // correctness" on the landing page).
            let mut fields = IndexMap::new();
            fields.insert("now".into(), Ty::function(
                vec![],
                EffectSet::singleton("time"),
                Ty::int(),
            ));
            Some(Ty::Record(fields))
        }
        "rand" => {
            // rand.int_in(lo, hi) -> [rand] Int — currently a deterministic
            // stub (midpoint) per spec §13; replaced when randomness lands.
            let mut fields = IndexMap::new();
            fields.insert("int_in".into(), Ty::function(
                vec![Ty::int(), Ty::int()],
                EffectSet::singleton("rand"),
                Ty::int(),
            ));
            Some(Ty::Record(fields))
        }
        "env" => {
            // #216: env.get(name) -> [env] Option[Str].
            // Per-var scoping (`[env(NAME)]`) lands with the
            // per-capability effect parameterization work (#207); the
            // flat `[env]` is the v1 surface.
            let mut fields = IndexMap::new();
            fields.insert("get".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("env"),
                Ty::Con("Option".into(), vec![Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        "net" => {
            let mut fields = IndexMap::new();
            // get :: Str -> [net] Result[Str, Str]
            fields.insert("get".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("net"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            fields.insert("post".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            // serve :: (Int, Str) -> [net] Unit  (blocks; never returns
            // under normal use). Handler's signature isn't carried in
            // the type system here — looked up by name at runtime.
            fields.insert("serve".into(), Ty::function(
                vec![Ty::int(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit,
            ));
            // serve_tls :: (Int, Str, Str, Str) -> [net] Unit
            //              port  cert  key   handler
            // cert and key are filesystem paths to PEM-encoded files.
            fields.insert("serve_tls".into(), Ty::function(
                vec![Ty::int(), Ty::str(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit,
            ));
            // serve_ws :: (Int, Str) -> [net] Unit
            //             port  on_message_handler_name
            // The handler is looked up by name at runtime.
            fields.insert("serve_ws".into(), Ty::function(
                vec![Ty::int(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit,
            ));
            Some(Ty::Record(fields))
        }
        "chat" => {
            let mut fields = IndexMap::new();
            fields.insert("broadcast".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::singleton("chat"),
                Ty::Unit,
            ));
            fields.insert("send".into(), Ty::function(
                vec![Ty::int(), Ty::str()],
                EffectSet::singleton("chat"),
                Ty::bool(),
            ));
            Some(Ty::Record(fields))
        }
        "proc" => {
            // Subprocess dispatch. Effect: [proc]. Returns a Result
            // with a record on success carrying stdout / stderr /
            // exit_code. The runtime allow-lists which binary
            // basenames are spawnable — `cmd` is the program to
            // run, `args` is the literal argv (no shell parsing).
            //
            // Read SECURITY.md before adding [proc] to a policy:
            // it weakens the "we know what this fn does" claim.
            let mut fields = IndexMap::new();
            let mut result_rec = IndexMap::new();
            result_rec.insert("stdout".into(), Ty::str());
            result_rec.insert("stderr".into(), Ty::str());
            result_rec.insert("exit_code".into(), Ty::int());
            // spawn :: Str, List[Str] -> [proc] Result[{stdout, stderr, exit_code}, Str]
            fields.insert("spawn".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::singleton("proc"),
                Ty::Con("Result".into(), vec![
                    Ty::Record(result_rec),
                    Ty::str(),
                ]),
            ));
            Some(Ty::Record(fields))
        }
        "json" => {
            let mut fields = IndexMap::new();
            // stringify :: T -> Str  (polymorphic on input)
            fields.insert("stringify".into(), Ty::function(
                vec![Ty::Var(0)], EffectSet::empty(), Ty::str(),
            ));
            // parse :: Str -> Result[T, Str]
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            // parse_strict :: (Str, List[Str]) -> Result[T, Str]
            // Tactical fix for #168 — caller passes the field names
            // T requires; runtime returns Err if any are missing
            // from the parsed object instead of letting field
            // access panic later.
            fields.insert("parse_strict".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        "result" => {
            let mut fields = IndexMap::new();
            // result.map :: Result[T, E], (T) -> [E2] U -> [E2] Result[U, E]
            // Effect-polymorphic on the closure: result.map et al.
            // propagate the closure's effects to the surrounding call.
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3), Ty::Var(2)),
                ],
                EffectSet::open_var(3),
                Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)]),
            ));
            fields.insert("and_then".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(4),
                        Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)])),
                ],
                EffectSet::open_var(4),
                Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)]),
            ));
            fields.insert("map_err".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(1)], EffectSet::open_var(5), Ty::Var(2)),
                ],
                EffectSet::open_var(5),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)]),
            ));
            // result.or_else :: Result[T, E1], (E1) -> [E] Result[T, E2]
            //                                    -> [E] Result[T, E2]
            // Recovery combinator: closure runs only on Err and returns
            // the next Result (which itself may swap the error type).
            fields.insert("or_else".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(1)], EffectSet::open_var(6),
                        Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)])),
                ],
                EffectSet::open_var(6),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)]),
            ));
            Some(Ty::Record(fields))
        }
        "option" => {
            let mut fields = IndexMap::new();
            // option.map :: Option[T], (T) -> [E] U -> [E] Option[U]
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::Con("Option".into(), vec![Ty::Var(0)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(2), Ty::Var(1)),
                ],
                EffectSet::open_var(2),
                Ty::Con("Option".into(), vec![Ty::Var(1)]),
            ));
            // option.and_then :: Option[T], (T) -> [E] Option[U] -> [E] Option[U]
            // The compiler entry has been wired since the result/option
            // variant_map work landed; this signature was missed,
            // making the call fail to type-check until now.
            fields.insert("and_then".into(), Ty::function(
                vec![
                    Ty::Con("Option".into(), vec![Ty::Var(0)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3),
                        Ty::Con("Option".into(), vec![Ty::Var(1)])),
                ],
                EffectSet::open_var(3),
                Ty::Con("Option".into(), vec![Ty::Var(1)]),
            ));
            fields.insert("unwrap_or".into(), Ty::function(
                vec![Ty::Con("Option".into(), vec![Ty::Var(0)]), Ty::Var(0)],
                EffectSet::empty(),
                Ty::Var(0),
            ));
            // option.or_else :: Option[T], () -> [E] Option[T] -> [E] Option[T]
            // The closure takes no arguments because None has no payload to pass.
            fields.insert("or_else".into(), Ty::function(
                vec![
                    Ty::Con("Option".into(), vec![Ty::Var(0)]),
                    Ty::function(vec![], EffectSet::open_var(4),
                        Ty::Con("Option".into(), vec![Ty::Var(0)])),
                ],
                EffectSet::open_var(4),
                Ty::Con("Option".into(), vec![Ty::Var(0)]),
            ));
            Some(Ty::Record(fields))
        }
        "tuple" => {
            // Tuple accessors per §11.1. Polymorphic in the tuple's
            // element types; we use the same row-variable shape used
            // by list helpers. Tuples are heterogeneous, so each
            // accessor is statically typed via independent type
            // variables for each position.
            let mut fields = IndexMap::new();
            // fst :: (T0, T1) -> T0
            fields.insert("fst".into(), Ty::function(
                vec![Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)])],
                EffectSet::empty(),
                Ty::Var(0),
            ));
            // snd :: (T0, T1) -> T1
            fields.insert("snd".into(), Ty::function(
                vec![Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)])],
                EffectSet::empty(),
                Ty::Var(1),
            ));
            // third :: (T0, T1, T2) -> T2
            fields.insert("third".into(), Ty::function(
                vec![Ty::Tuple(vec![Ty::Var(0), Ty::Var(1), Ty::Var(2)])],
                EffectSet::empty(),
                Ty::Var(2),
            ));
            // len :: (T0, T1) -> Int  (covers any pair shape; Int back)
            fields.insert("len".into(), Ty::function(
                vec![Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)])],
                EffectSet::empty(),
                Ty::int(),
            ));
            Some(Ty::Record(fields))
        }
        "map" => {
            // Persistent map. Keys are `Str` or `Int` only — Lex's
            // type system tracks them polymorphically as Var(0)
            // ("K") and lets the runtime check the key shape; both
            // cases fit into `MapKey`.
            //
            // Type variables: 0 = K, 1 = V.
            let mt   = || Ty::Con("Map".into(), vec![Ty::Var(0), Ty::Var(1)]);
            let pair = || Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)]);
            let mut fields = IndexMap::new();
            // new :: () -> Map[K, V]
            fields.insert("new".into(), Ty::function(
                vec![], EffectSet::empty(), mt()));
            // size :: Map[K, V] -> Int
            fields.insert("size".into(), Ty::function(
                vec![mt()], EffectSet::empty(), Ty::int()));
            // has :: Map[K, V], K -> Bool
            fields.insert("has".into(), Ty::function(
                vec![mt(), Ty::Var(0)], EffectSet::empty(), Ty::bool()));
            // get :: Map[K, V], K -> Option[V]
            fields.insert("get".into(), Ty::function(
                vec![mt(), Ty::Var(0)], EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::Var(1)])));
            // set :: Map[K, V], K, V -> Map[K, V]
            fields.insert("set".into(), Ty::function(
                vec![mt(), Ty::Var(0), Ty::Var(1)],
                EffectSet::empty(), mt()));
            // delete :: Map[K, V], K -> Map[K, V]
            fields.insert("delete".into(), Ty::function(
                vec![mt(), Ty::Var(0)], EffectSet::empty(), mt()));
            // keys :: Map[K, V] -> List[K]
            fields.insert("keys".into(), Ty::function(
                vec![mt()], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0)))));
            // values :: Map[K, V] -> List[V]
            fields.insert("values".into(), Ty::function(
                vec![mt()], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(1)))));
            // entries :: Map[K, V] -> List[(K, V)]
            fields.insert("entries".into(), Ty::function(
                vec![mt()], EffectSet::empty(),
                Ty::List(Box::new(pair()))));
            // from_list :: List[(K, V)] -> Map[K, V]
            fields.insert("from_list".into(), Ty::function(
                vec![Ty::List(Box::new(pair()))],
                EffectSet::empty(), mt()));
            // merge :: Map[K, V], Map[K, V] -> Map[K, V]   (b overrides a)
            fields.insert("merge".into(), Ty::function(
                vec![mt(), mt()], EffectSet::empty(), mt()));
            // is_empty :: Map[K, V] -> Bool
            fields.insert("is_empty".into(), Ty::function(
                vec![mt()], EffectSet::empty(), Ty::bool()));
            // fold :: Map[K, V], A, (A, K, V) -> [E] A -> [E] A
            // Iteration order matches `map.entries` (BTreeMap-sorted by
            // key). Effect-polymorphic on the combiner like `list.fold`.
            // Type variable 2 = A (accumulator), effect row 3.
            fields.insert("fold".into(), Ty::function(
                vec![
                    mt(),
                    Ty::Var(2),
                    Ty::function(
                        vec![Ty::Var(2), Ty::Var(0), Ty::Var(1)],
                        EffectSet::open_var(3),
                        Ty::Var(2),
                    ),
                ],
                EffectSet::open_var(3),
                Ty::Var(2),
            ));
            Some(Ty::Record(fields))
        }
        "set" => {
            // Persistent set with the same key-type discipline as map.
            // Type variable: 0 = T (the element type, also the key type).
            let st   = || Ty::Con("Set".into(), vec![Ty::Var(0)]);
            let mut fields = IndexMap::new();
            // new :: () -> Set[T]
            fields.insert("new".into(), Ty::function(
                vec![], EffectSet::empty(), st()));
            // size :: Set[T] -> Int
            fields.insert("size".into(), Ty::function(
                vec![st()], EffectSet::empty(), Ty::int()));
            // has :: Set[T], T -> Bool
            fields.insert("has".into(), Ty::function(
                vec![st(), Ty::Var(0)], EffectSet::empty(), Ty::bool()));
            // add :: Set[T], T -> Set[T]
            fields.insert("add".into(), Ty::function(
                vec![st(), Ty::Var(0)], EffectSet::empty(), st()));
            // delete :: Set[T], T -> Set[T]
            fields.insert("delete".into(), Ty::function(
                vec![st(), Ty::Var(0)], EffectSet::empty(), st()));
            // to_list :: Set[T] -> List[T]
            fields.insert("to_list".into(), Ty::function(
                vec![st()], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0)))));
            // from_list :: List[T] -> Set[T]
            fields.insert("from_list".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(), st()));
            // union :: Set[T], Set[T] -> Set[T]
            fields.insert("union".into(), Ty::function(
                vec![st(), st()], EffectSet::empty(), st()));
            // intersect :: Set[T], Set[T] -> Set[T]
            fields.insert("intersect".into(), Ty::function(
                vec![st(), st()], EffectSet::empty(), st()));
            // diff :: Set[T], Set[T] -> Set[T]
            fields.insert("diff".into(), Ty::function(
                vec![st(), st()], EffectSet::empty(), st()));
            // is_empty :: Set[T] -> Bool
            fields.insert("is_empty".into(), Ty::function(
                vec![st()], EffectSet::empty(), Ty::bool()));
            // is_subset :: Set[T], Set[T] -> Bool   (a is subset of b)
            fields.insert("is_subset".into(), Ty::function(
                vec![st(), st()], EffectSet::empty(), Ty::bool()));
            Some(Ty::Record(fields))
        }
        "flow" => {
            // Orchestration primitives (spec §11.2). Each takes one or
            // more closures and returns a closure with a derived shape.
            let mut fields = IndexMap::new();
            // sequential[T, U, V](f: (T) -> U, g: (U) -> V) -> (T) -> V
            fields.insert("sequential".into(), Ty::function(
                vec![
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                    Ty::function(vec![Ty::Var(1)], EffectSet::empty(), Ty::Var(2)),
                ],
                EffectSet::empty(),
                Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(2)),
            ));
            // branch[T, U](cond: (T) -> Bool, t: (T) -> U, f: (T) -> U) -> (T) -> U
            fields.insert("branch".into(), Ty::function(
                vec![
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::bool()),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                ],
                EffectSet::empty(),
                Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
            ));
            // retry[T, U, E](f: (T) -> Result[U, E], n: Int) -> (T) -> Result[U, E]
            let result_ty = Ty::Con("Result".into(), vec![Ty::Var(1), Ty::Var(2)]);
            fields.insert("retry".into(), Ty::function(
                vec![
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), result_ty.clone()),
                    Ty::int(),
                ],
                EffectSet::empty(),
                Ty::function(vec![Ty::Var(0)], EffectSet::empty(), result_ty),
            ));
            // parallel[A, B](fa: () -> A, fb: () -> B) -> () -> (A, B)
            // Sequential implementation today; spec §11.2 reserves the
            // option of a true-threaded scheduler. parallel_record is
            // listed in the spec but not yet implemented — it needs row
            // polymorphism over the input record's fields plus a
            // record-iteration trampoline; tracked as follow-up.
            fields.insert("parallel".into(), Ty::function(
                vec![
                    Ty::function(vec![], EffectSet::empty(), Ty::Var(0)),
                    Ty::function(vec![], EffectSet::empty(), Ty::Var(1)),
                ],
                EffectSet::empty(),
                Ty::function(vec![], EffectSet::empty(),
                    Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)])),
            ));
            // parallel_list[T](actions: List[() -> T]) -> List[T]
            // Variadic counterpart to `parallel`. Runs each action and
            // collects results in input order. Sequential under the
            // hood (same caveat as `parallel`); spec §11.2 reserves
            // true threading for a future scheduler. Unlike `parallel`,
            // this returns the result list directly rather than a
            // closure, since the input arity is dynamic.
            fields.insert("parallel_list".into(), Ty::function(
                vec![
                    Ty::List(Box::new(
                        Ty::function(vec![], EffectSet::empty(), Ty::Var(0)),
                    )),
                ],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            Some(Ty::Record(fields))
        }
        "crypto" => {
            let mut fields = IndexMap::new();
            // Hashes: Bytes -> Bytes (digest as raw bytes)
            for name in &["sha256", "sha512", "md5"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::bytes()],
                    EffectSet::empty(),
                    Ty::bytes(),
                ));
            }
            // HMAC: (key :: Bytes, data :: Bytes) -> Bytes
            for name in &["hmac_sha256", "hmac_sha512"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::bytes(), Ty::bytes()],
                    EffectSet::empty(),
                    Ty::bytes(),
                ));
            }
            // base64 / hex
            fields.insert("base64_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("base64_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            fields.insert("hex_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("hex_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            // constant-time equality (for HMAC verification etc.)
            fields.insert("constant_time_eq".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()], EffectSet::empty(), Ty::bool()));
            // Cryptographically-secure random bytes — OS RNG, not the
            // deterministic `rand.int_in` stub. The new `[random]`
            // effect is fine-grained on purpose so reviewers can find
            // every token-generating call via `lex audit --effect
            // random`.
            fields.insert("random".into(), Ty::function(
                vec![Ty::int()],
                EffectSet::singleton("random"),
                Ty::bytes(),
            ));
            Some(Ty::Record(fields))
        }
        "deque" => {
            // Persistent double-ended queue. Push/pop O(1) on both
            // ends; iteration order is front-to-back.
            // Type variable: 0 = T.
            let dt   = || Ty::Con("Deque".into(), vec![Ty::Var(0)]);
            let pair = || Ty::Tuple(vec![Ty::Var(0), dt()]);
            let mut fields = IndexMap::new();
            // new :: () -> Deque[T]
            fields.insert("new".into(), Ty::function(
                vec![], EffectSet::empty(), dt()));
            // size :: Deque[T] -> Int
            fields.insert("size".into(), Ty::function(
                vec![dt()], EffectSet::empty(), Ty::int()));
            // is_empty :: Deque[T] -> Bool
            fields.insert("is_empty".into(), Ty::function(
                vec![dt()], EffectSet::empty(), Ty::bool()));
            // push_back / push_front :: Deque[T], T -> Deque[T]
            for n in &["push_back", "push_front"] {
                fields.insert((*n).into(), Ty::function(
                    vec![dt(), Ty::Var(0)], EffectSet::empty(), dt()));
            }
            // pop_back / pop_front :: Deque[T] -> Option[(T, Deque[T])]
            for n in &["pop_back", "pop_front"] {
                fields.insert((*n).into(), Ty::function(
                    vec![dt()], EffectSet::empty(),
                    Ty::Con("Option".into(), vec![pair()])));
            }
            // peek_back / peek_front :: Deque[T] -> Option[T]
            for n in &["peek_back", "peek_front"] {
                fields.insert((*n).into(), Ty::function(
                    vec![dt()], EffectSet::empty(),
                    Ty::Con("Option".into(), vec![Ty::Var(0)])));
            }
            // from_list :: List[T] -> Deque[T]
            fields.insert("from_list".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(), dt()));
            // to_list :: Deque[T] -> List[T]
            fields.insert("to_list".into(), Ty::function(
                vec![dt()], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0)))));
            Some(Ty::Record(fields))
        }
        "log" => {
            // Structured logging behind a [log] effect. Emit ops route
            // through a runtime-configured sink (stderr by default;
            // can be redirected via set_sink). Configuration ops
            // mutate the global sink and so are gated [io].
            let result_str = |t: Ty| Ty::Con("Result".into(), vec![t, Ty::str()]);
            let mut fields = IndexMap::new();
            for level in &["debug", "info", "warn", "error"] {
                fields.insert((*level).into(), Ty::function(
                    vec![Ty::str()],
                    EffectSet::singleton("log"),
                    Ty::Unit,
                ));
            }
            // set_level :: Str -> [io] Result[Nil, Str]
            fields.insert("set_level".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("io"),
                result_str(Ty::Unit)));
            // set_format :: Str -> [io] Result[Nil, Str]
            fields.insert("set_format".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("io"),
                result_str(Ty::Unit)));
            // set_sink :: Str -> [io, fs_write] Result[Nil, Str]
            fields.insert("set_sink".into(), Ty::function(
                vec![Ty::str()],
                EffectSet {
                    concrete: ["io".to_string(), "fs_write".to_string()].into_iter().collect(),
                    var: None,
                },
                result_str(Ty::Unit)));
            Some(Ty::Record(fields))
        }
        "datetime" => {
            // Instant and Duration are nominal opaque Ints under the
            // hood (nanoseconds-since-UTC-epoch and signed nanoseconds
            // respectively); the type checker tracks the distinction
            // even though both values look like Int at runtime.
            //
            // Tz is the variant
            //     Utc | Local | Offset(Int) | Iana(Str)
            // registered as a built-in nominal type in
            // `TypeEnv::new_with_builtins`. The pre-v1 stringly Tz
            // ("UTC"/"Local"/IANA-name/"+05:30") is no longer accepted
            // — passing a `Str` to `to_components` is now a type
            // error.
            let inst   = || Ty::Con("Instant".into(), vec![]);
            let dur    = || Ty::Con("Duration".into(), vec![]);
            let tz     = || Ty::Con("Tz".into(), vec![]);
            let result_str = |t: Ty| Ty::Con("Result".into(), vec![t, Ty::str()]);
            let dt_t = || {
                let mut fs = IndexMap::new();
                fs.insert("year".into(),    Ty::int());
                fs.insert("month".into(),   Ty::int());
                fs.insert("day".into(),     Ty::int());
                fs.insert("hour".into(),    Ty::int());
                fs.insert("minute".into(),  Ty::int());
                fs.insert("second".into(),  Ty::int());
                fs.insert("nano".into(),    Ty::int());
                fs.insert("tz_offset_minutes".into(), Ty::int());
                Ty::Record(fs)
            };
            let mut fields = IndexMap::new();
            fields.insert("now".into(), Ty::function(
                vec![], EffectSet::singleton("time"), inst()));
            fields.insert("parse_iso".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(), result_str(inst())));
            fields.insert("format_iso".into(), Ty::function(
                vec![inst()], EffectSet::empty(), Ty::str()));
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str(), Ty::str()], EffectSet::empty(), result_str(inst())));
            fields.insert("format".into(), Ty::function(
                vec![inst(), Ty::str()], EffectSet::empty(), Ty::str()));
            fields.insert("to_components".into(), Ty::function(
                vec![inst(), tz()], EffectSet::empty(), result_str(dt_t())));
            fields.insert("from_components".into(), Ty::function(
                vec![dt_t()], EffectSet::empty(), result_str(inst())));
            fields.insert("add".into(), Ty::function(
                vec![inst(), dur()], EffectSet::empty(), inst()));
            fields.insert("diff".into(), Ty::function(
                vec![inst(), inst()], EffectSet::empty(), dur()));
            fields.insert("duration_seconds".into(), Ty::function(
                vec![Ty::float()], EffectSet::empty(), dur()));
            fields.insert("duration_minutes".into(), Ty::function(
                vec![Ty::int()], EffectSet::empty(), dur()));
            fields.insert("duration_days".into(), Ty::function(
                vec![Ty::int()], EffectSet::empty(), dur()));
            Some(Ty::Record(fields))
        }
        "process" => {
            // Streaming subprocess. The opaque `ProcessHandle` type
            // is an Int handle into a process-wide registry holding
            // the `Child` plus its stdout/stderr `BufReader`s.
            let ph = || Ty::Con("ProcessHandle".into(), vec![]);
            let result_str = |t: Ty| Ty::Con("Result".into(), vec![t, Ty::str()]);
            let opts_t = || {
                let mut fs = IndexMap::new();
                fs.insert("cwd".into(),
                    Ty::Con("Option".into(), vec![Ty::str()]));
                fs.insert("env".into(),
                    Ty::Con("Map".into(), vec![Ty::str(), Ty::str()]));
                fs.insert("stdin".into(),
                    Ty::Con("Option".into(), vec![Ty::bytes()]));
                Ty::Record(fs)
            };
            let exit_t = || {
                let mut fs = IndexMap::new();
                fs.insert("code".into(), Ty::int());
                fs.insert("signaled".into(), Ty::bool());
                Ty::Record(fs)
            };
            let output_t = || {
                let mut fs = IndexMap::new();
                fs.insert("stdout".into(), Ty::str());
                fs.insert("stderr".into(), Ty::str());
                fs.insert("exit_code".into(), Ty::int());
                Ty::Record(fs)
            };
            let mut fields = IndexMap::new();
            // spawn :: Str, List[Str], Opts -> [proc] Result[ProcessHandle, Str]
            fields.insert("spawn".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str())), opts_t()],
                EffectSet::singleton("proc"),
                result_str(ph())));
            // read_stdout_line / read_stderr_line :: ProcessHandle -> [proc] Option[Str]
            for n in &["read_stdout_line", "read_stderr_line"] {
                fields.insert((*n).into(), Ty::function(
                    vec![ph()], EffectSet::singleton("proc"),
                    Ty::Con("Option".into(), vec![Ty::str()])));
            }
            // wait :: ProcessHandle -> [proc] ProcessExit
            fields.insert("wait".into(), Ty::function(
                vec![ph()], EffectSet::singleton("proc"), exit_t()));
            // kill :: ProcessHandle, Str -> [proc] Result[Nil, Str]
            fields.insert("kill".into(), Ty::function(
                vec![ph(), Ty::str()],
                EffectSet::singleton("proc"),
                result_str(Ty::Unit)));
            // run :: Str, List[Str] -> [proc] Result[ProcessOutput, Str]
            // Blocking convenience that captures stdout/stderr fully
            // and returns once the child exits. For programs that
            // need streaming, use spawn + read_*_line + wait.
            fields.insert("run".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::singleton("proc"),
                result_str(output_t())));
            Some(Ty::Record(fields))
        }
        "fs" => {
            // Filesystem walk + mutate. Walk-style ops (exists, walk,
            // glob, …) declare [fs_walk] — distinct from [fs_read]
            // (which is content reads via io.read), so reviewers can
            // separately track directory traversal vs file-content
            // exposure. Mutating ops (mkdir_p, remove, copy) declare
            // [fs_write]. Path scoping uses --allow-fs-read for walk
            // (a directory listing is an information disclosure on
            // the same path tree) and --allow-fs-write for mutations.
            let stat_t = || {
                let mut fs = IndexMap::new();
                fs.insert("size".into(), Ty::int());
                fs.insert("mtime".into(), Ty::int());
                fs.insert("is_dir".into(), Ty::bool());
                fs.insert("is_file".into(), Ty::bool());
                Ty::Record(fs)
            };
            let result_str = |t: Ty| Ty::Con("Result".into(), vec![t, Ty::str()]);
            let mut fields = IndexMap::new();
            // Walk-style queries [fs_walk]
            fields.insert("exists".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"), Ty::bool()));
            fields.insert("is_file".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"), Ty::bool()));
            fields.insert("is_dir".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"), Ty::bool()));
            fields.insert("stat".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"),
                result_str(stat_t())));
            fields.insert("list_dir".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"),
                result_str(Ty::List(Box::new(Ty::str())))));
            fields.insert("walk".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"),
                result_str(Ty::List(Box::new(Ty::str())))));
            fields.insert("glob".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_walk"),
                result_str(Ty::List(Box::new(Ty::str())))));
            // Mutations [fs_write]
            fields.insert("mkdir_p".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_write"),
                result_str(Ty::Unit)));
            fields.insert("remove".into(), Ty::function(
                vec![Ty::str()], EffectSet::singleton("fs_write"),
                result_str(Ty::Unit)));
            fields.insert("copy".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet {
                    concrete: ["fs_walk".to_string(), "fs_write".to_string()].into_iter().collect(),
                    var: None,
                },
                result_str(Ty::Unit)));
            Some(Ty::Record(fields))
        }
        "kv" => {
            // Embedded key-value store. The opaque `Kv` type is
            // backed by an Int handle into a process-wide registry.
            let kv_t = || Ty::Con("Kv".into(), vec![]);
            let mut fields = IndexMap::new();
            // open :: Str -> [kv, fs_write] Result[Kv, Str]
            fields.insert("open".into(), Ty::function(
                vec![Ty::str()],
                EffectSet {
                    concrete: ["kv".to_string(), "fs_write".to_string()].into_iter().collect(),
                    var: None,
                },
                Ty::Con("Result".into(), vec![kv_t(), Ty::str()])));
            // close :: Kv -> [kv] Nil
            fields.insert("close".into(), Ty::function(
                vec![kv_t()],
                EffectSet::singleton("kv"),
                Ty::Unit));
            // get :: Kv, Str -> [kv] Option[Bytes]
            fields.insert("get".into(), Ty::function(
                vec![kv_t(), Ty::str()],
                EffectSet::singleton("kv"),
                Ty::Con("Option".into(), vec![Ty::bytes()])));
            // put :: Kv, Str, Bytes -> [kv] Result[Nil, Str]
            fields.insert("put".into(), Ty::function(
                vec![kv_t(), Ty::str(), Ty::bytes()],
                EffectSet::singleton("kv"),
                Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()])));
            // delete :: Kv, Str -> [kv] Result[Nil, Str]
            fields.insert("delete".into(), Ty::function(
                vec![kv_t(), Ty::str()],
                EffectSet::singleton("kv"),
                Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()])));
            // contains :: Kv, Str -> [kv] Bool
            fields.insert("contains".into(), Ty::function(
                vec![kv_t(), Ty::str()],
                EffectSet::singleton("kv"),
                Ty::bool()));
            // list_prefix :: Kv, Str -> [kv] List[Str]
            fields.insert("list_prefix".into(), Ty::function(
                vec![kv_t(), Ty::str()],
                EffectSet::singleton("kv"),
                Ty::List(Box::new(Ty::str()))));
            Some(Ty::Record(fields))
        }
        "sql" => {
            // Embedded SQL (SQLite). The opaque `Db` type is backed
            // by an Int handle into a process-wide registry, same
            // shape as `Kv`. v1 surface focuses on read-heavy and
            // simple-write workloads — the kind that drove the
            // requirement (audit history, "filter by verdict where
            // score > 60", joins). Transactions, heterogeneous
            // typed parameter binding, and named params are
            // deferred to v1.5.
            //
            // Params are `List[Str]` for v1: callers stringify Int /
            // Float values before binding, and SQLite's column type
            // affinity coerces back at insert time. This is the one
            // honest ergonomics caveat; the alternative (a tagged
            // `SqlValue` variant) is forward-compatible but adds a
            // type to the global scope that v1 doesn't need.
            let db_t = || Ty::Con("Db".into(), vec![]);
            let mut fields = IndexMap::new();
            // open :: Str -> [sql, fs_write] Result[Db, Str]
            // Path is the SQLite filename; ":memory:" works for
            // ephemeral stores. fs_write is required because the
            // DB file is created on first open.
            fields.insert("open".into(), Ty::function(
                vec![Ty::str()],
                EffectSet {
                    concrete: ["sql".to_string(), "fs_write".to_string()].into_iter().collect(),
                    var: None,
                },
                Ty::Con("Result".into(), vec![db_t(), Ty::str()])));
            // close :: Db -> [sql] Nil
            fields.insert("close".into(), Ty::function(
                vec![db_t()],
                EffectSet::singleton("sql"),
                Ty::Unit));
            // exec :: Db, Str, List[Str] -> [sql] Result[Int, Str]
            // Returns the affected row count (rusqlite's `execute`).
            // Suitable for INSERT / UPDATE / DELETE / DDL.
            fields.insert("exec".into(), Ty::function(
                vec![db_t(), Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![Ty::int(), Ty::str()])));
            // query[T] :: Db, Str, List[Str] -> [sql] Result[List[T], Str]
            // Polymorphic on the row record shape. Each row is
            // decoded into a record keyed by column name, with
            // SQLite values mapped to the same Lex `Value` shape
            // as `json.parse` and `toml.parse` produce.
            fields.insert("query".into(), Ty::function(
                vec![db_t(), Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::str(),
                ])));
            Some(Ty::Record(fields))
        }
        "regex" => {
            // The compiled `Regex` is stored as a `Str` at runtime
            // (the pattern source) plus a process-wide cache of the
            // actual `regex::Regex`. So `Regex` is a nominal type at
            // the language level but its value is just the pattern.
            let regex_t = || Ty::Con("Regex".into(), vec![]);
            let match_t = || {
                let mut fs = IndexMap::new();
                fs.insert("text".into(), Ty::str());
                fs.insert("start".into(), Ty::int());
                fs.insert("end".into(), Ty::int());
                fs.insert("groups".into(), Ty::List(Box::new(Ty::str())));
                Ty::Record(fs)
            };
            let mut fields = IndexMap::new();
            // compile :: Str -> Result[Regex, Str]
            fields.insert("compile".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![regex_t(), Ty::str()])));
            // is_match :: Regex, Str -> Bool
            fields.insert("is_match".into(), Ty::function(
                vec![regex_t(), Ty::str()], EffectSet::empty(), Ty::bool()));
            // find :: Regex, Str -> Option[Match]
            fields.insert("find".into(), Ty::function(
                vec![regex_t(), Ty::str()], EffectSet::empty(),
                Ty::Con("Option".into(), vec![match_t()])));
            // find_all :: Regex, Str -> List[Match]
            fields.insert("find_all".into(), Ty::function(
                vec![regex_t(), Ty::str()], EffectSet::empty(),
                Ty::List(Box::new(match_t()))));
            // replace :: Regex, Str, Str -> Str
            fields.insert("replace".into(), Ty::function(
                vec![regex_t(), Ty::str(), Ty::str()], EffectSet::empty(), Ty::str()));
            // replace_all :: Regex, Str, Str -> Str
            fields.insert("replace_all".into(), Ty::function(
                vec![regex_t(), Ty::str(), Ty::str()], EffectSet::empty(), Ty::str()));
            // split :: Regex, Str -> List[Str]
            fields.insert("split".into(), Ty::function(
                vec![regex_t(), Ty::str()], EffectSet::empty(),
                Ty::List(Box::new(Ty::str()))));
            Some(Ty::Record(fields))
        }
        "http" => {
            // Rich HTTP client. `[net]` for the wire ops, pure for
            // the builders / decoders. `--allow-net-host` gates per
            // request. Multipart upload + streaming response bodies
            // are deferred to v1.5; the v1 surface covers the
            // common cases (auth, headers, query, timeouts, JSON /
            // text decoding).
            let req_t  = || Ty::Con("HttpRequest".into(), vec![]);
            let resp_t = || Ty::Con("HttpResponse".into(), vec![]);
            let err_t  = || Ty::Con("HttpError".into(), vec![]);
            let result_he = |t: Ty| Ty::Con("Result".into(), vec![t, err_t()]);
            let str_str_map = || Ty::Con("Map".into(), vec![Ty::str(), Ty::str()]);
            let mut fields = IndexMap::new();
            // -- wire ops (effectful) --
            // send :: HttpRequest -> [net] Result[HttpResponse, HttpError]
            fields.insert("send".into(), Ty::function(
                vec![req_t()],
                EffectSet::singleton("net"),
                result_he(resp_t()),
            ));
            // get :: Str -> [net] Result[HttpResponse, HttpError]
            fields.insert("get".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("net"),
                result_he(resp_t()),
            ));
            // post :: Str, Bytes, Str -> [net] Result[HttpResponse, HttpError]
            fields.insert("post".into(), Ty::function(
                vec![Ty::str(), Ty::bytes(), Ty::str()],
                EffectSet::singleton("net"),
                result_he(resp_t()),
            ));
            // -- pure builders (record transforms) --
            // with_header :: HttpRequest, Str, Str -> HttpRequest
            fields.insert("with_header".into(), Ty::function(
                vec![req_t(), Ty::str(), Ty::str()],
                EffectSet::empty(),
                req_t(),
            ));
            // with_auth :: HttpRequest, Str, Str -> HttpRequest
            // (Renders `<scheme> <token>` into the `Authorization`
            // header — `Bearer <jwt>`, `Basic <b64>`, etc.)
            fields.insert("with_auth".into(), Ty::function(
                vec![req_t(), Ty::str(), Ty::str()],
                EffectSet::empty(),
                req_t(),
            ));
            // with_query :: HttpRequest, Map[Str, Str] -> HttpRequest
            // (Appends a `?k=v&...` query string; values are URL-
            // encoded so `&` / `=` / spaces in values don't escape.)
            fields.insert("with_query".into(), Ty::function(
                vec![req_t(), str_str_map()],
                EffectSet::empty(),
                req_t(),
            ));
            // with_timeout_ms :: HttpRequest, Int -> HttpRequest
            fields.insert("with_timeout_ms".into(), Ty::function(
                vec![req_t(), Ty::int()],
                EffectSet::empty(),
                req_t(),
            ));
            // -- pure decoders --
            // json_body[T] :: HttpResponse -> Result[T, HttpError]
            // Polymorphic on the parsed shape, matching `json.parse`.
            fields.insert("json_body".into(), Ty::function(
                vec![resp_t()],
                EffectSet::empty(),
                result_he(Ty::Var(0)),
            ));
            // text_body :: HttpResponse -> Result[Str, HttpError]
            fields.insert("text_body".into(), Ty::function(
                vec![resp_t()],
                EffectSet::empty(),
                result_he(Ty::str()),
            ));
            Some(Ty::Record(fields))
        }
        "yaml" => {
            // YAML config parser. Same shape as `std.toml`: parse
            // is polymorphic, output Value layout matches std.json
            // (Str/Int/Float/Bool/List/Record). Anchors and tags
            // are flattened by serde_yaml's deserializer.
            let mut fields = IndexMap::new();
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            // Tactical fix for #168 — caller-supplied required-field
            // list. See std.json's parse_strict for context.
            fields.insert("parse_strict".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            fields.insert("stringify".into(), Ty::function(
                vec![Ty::Var(0)], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        "dotenv" => {
            // .env-style files. parse :: Str -> Result[Map[Str,Str], Str].
            // Returns a map (not a polymorphic record) because
            // dotenv files don't carry shape — every value is a
            // string and keys aren't statically known.
            let mut fields = IndexMap::new();
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![
                    Ty::Con("Map".into(), vec![Ty::str(), Ty::str()]),
                    Ty::str(),
                ]),
            ));
            Some(Ty::Record(fields))
        }
        "csv" => {
            // CSV rows-as-lists. parse :: Str -> Result[List[List[Str]], Str].
            // Header awareness is left to the caller — row 0 is
            // whatever the file has. A `parse_with_headers` that
            // returns List[Map[Str,Str]] is a natural follow-up.
            let row_ty = Ty::List(Box::new(Ty::str()));
            let rows_ty = Ty::List(Box::new(row_ty.clone()));
            let mut fields = IndexMap::new();
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![rows_ty.clone(), Ty::str()]),
            ));
            fields.insert("stringify".into(), Ty::function(
                vec![rows_ty], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        "test" => {
            // Tiny assertion library (#proposed-stdlib). Each helper
            // returns Result[Unit, Str] so a test is itself a fn
            // returning Result. Callers compose suites in user code
            // (a List of (name, () -> Result[Unit, Str]) pairs +
            // list.fold to accumulate verdicts). Property generators
            // and a Rust-side Suite type are deferred to v2.
            let mut fields = IndexMap::new();
            // assert_eq[a, b] :: T -> T -> Result[Unit, Str]
            // (T constrained equal by unification on the two args)
            let unit_result = || Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()]);
            fields.insert("assert_eq".into(), Ty::function(
                vec![Ty::Var(0), Ty::Var(0)], EffectSet::empty(), unit_result(),
            ));
            fields.insert("assert_ne".into(), Ty::function(
                vec![Ty::Var(0), Ty::Var(0)], EffectSet::empty(), unit_result(),
            ));
            fields.insert("assert_true".into(), Ty::function(
                vec![Ty::bool()], EffectSet::empty(), unit_result(),
            ));
            fields.insert("assert_false".into(), Ty::function(
                vec![Ty::bool()], EffectSet::empty(), unit_result(),
            ));
            Some(Ty::Record(fields))
        }
        "toml" => {
            // TOML config parser. Mirrors `std.json`'s shape: parse
            // is polymorphic so callers annotate the expected
            // record / list / scalar shape and the type checker
            // unifies. The parsed TOML maps to the same Lex Value
            // shape as JSON does:
            //
            //   TOML String   → Value::Str
            //   TOML Integer  → Value::Int
            //   TOML Float    → Value::Float
            //   TOML Boolean  → Value::Bool
            //   TOML Array    → Value::List
            //   TOML Table    → Value::Record
            //   TOML Datetime → Value::Str (RFC 3339, lossless)
            //
            // The Datetime → Str fallback is the one info-losing
            // step; callers who want a real `Instant` can pipe the
            // string through `datetime.parse_iso`.
            let mut fields = IndexMap::new();
            // parse :: Str -> Result[T, Str]
            fields.insert("parse".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            // parse_strict :: (Str, List[Str]) -> Result[T, Str]
            // Tactical fix for #168 — caller passes the field
            // names T requires; runtime returns Err if any are
            // missing from the parsed table instead of letting
            // field access panic later. The full type-driven fix
            // (deriving `required` from T at type-check time so
            // plain `parse[T]` validates) is tracked in #168.
            fields.insert("parse_strict".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::str()]),
            ));
            // stringify :: T -> Result[Str, Str]
            // Returns Result (not Str) because not every Lex Value
            // has a TOML representation — top-level scalars,
            // closures, mixed-key maps etc. surface as Err rather
            // than panic.
            fields.insert("stringify".into(), Ty::function(
                vec![Ty::Var(0)], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        // `std.agent` (#184) — runtime primitives whose effects
        // separate (a) which LLM surface (`llm_local` vs
        // `llm_cloud`), (b) which peer protocol (`a2a`), and
        // (c) which tool boundary (`mcp`). The wire formats land
        // in downstream crates (`soft-agent`, `soft-a2a`) and
        // in #185 for MCP; what's typed here is the boundary
        // alone — agent code can be type-checked as
        // `[llm_local, a2a]` and will fail if it tries to reach
        // `[llm_cloud]` even before the wire layer is finished.
        "agent" => {
            let mut fields = IndexMap::new();
            // local_complete :: Str -> [llm_local] Result[Str, Str]
            fields.insert("local_complete".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("llm_local"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            // cloud_complete :: Str -> [llm_cloud] Result[Str, Str]
            fields.insert("cloud_complete".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("llm_cloud"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            // send_a2a :: (Str, Str) -> [a2a] Result[Str, Str]
            //              peer payload                   reply
            fields.insert("send_a2a".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::singleton("a2a"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            // call_mcp :: (Str, Str, Str) -> [mcp] Result[Str, Str]
            //              server tool args_json         result_json
            fields.insert("call_mcp".into(), Ty::function(
                vec![Ty::str(), Ty::str(), Ty::str()],
                EffectSet::singleton("mcp"),
                Ty::Con("Result".into(), vec![Ty::str(), Ty::str()]),
            ));
            Some(Ty::Record(fields))
        }
        _ => None,
    }
}

/// Resolve `import "std.foo" as alias` to a module name (e.g. "io").
pub fn module_for_import(reference: &str) -> Option<&'static str> {
    let suffix = reference.strip_prefix("std.")?;
    Some(match suffix {
        "io" => "io",
        "str" => "str",
        "int" => "int",
        "float" => "float",
        "list" => "list",
        "result" => "result",
        "option" => "option",
        "json" => "json",
        "flow" => "flow",
        "tuple" => "tuple",
        "time" => "time",
        "rand" => "rand",
        "env" => "env",
        "bytes" => "bytes",
        "net" => "net",
        "chat" => "chat",
        "math" => "math",
        "map" => "map",
        "set" => "set",
        "proc" => "proc",
        "crypto" => "crypto",
        "regex" => "regex",
        "deque" => "deque",
        "kv" => "kv",
        "sql" => "sql",
        "fs" => "fs",
        "process" => "process",
        "datetime" => "datetime",
        "log" => "log",
        "http" => "http",
        "toml" => "toml",
        "yaml" => "yaml",
        "dotenv" => "dotenv",
        "csv" => "csv",
        "test" => "test",
        "agent" => "agent",
        _ => return None,
    })
}
