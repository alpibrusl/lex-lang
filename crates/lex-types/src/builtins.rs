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
            // io.readline() -> [io] Option[Str]
            fields.insert("readline".into(), Ty::function(
                vec![],
                EffectSet::singleton("io"),
                Ty::Con("Option".into(), vec![Ty::str()]),
            ));
            // io.argv() -> [io] List[Str]
            fields.insert("argv".into(), Ty::function(
                vec![],
                EffectSet::singleton("io"),
                Ty::List(Box::new(Ty::str())),
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
            // char_at :: (Str, Int) -> Str  — O(1) single-char access; "" if out of range.
            fields.insert("char_at".into(), Ty::function(vec![Ty::str(), Ty::int()], EffectSet::empty(), Ty::str()));
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
            // str.cmp :: (Str, Str) -> Int — -1 / 0 / 1, lex-byte order.
            // Three-way comparator usable as a sort-by closure value;
            // boolean comparisons stay on the `<`/`<=`/`>`/`>=` operators
            // (which already work on Str via `bin_ord`), so `str.lt`
            // etc. are deliberately not exposed — keep the surface
            // minimal (#440).
            fields.insert("cmp".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::empty(),
                Ty::int(),
            ));
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
            // Int scalar helpers (#681). math.{min,max,abs} are Float-only;
            // these are the integer equivalents so Int callers don't have to
            // round-trip through Float (which is lossy for large ints).
            fields.insert("abs".into(), Ty::function(vec![Ty::int()], EffectSet::empty(), Ty::int()));
            for name in &["min", "max"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()));
            }
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
            // #305 slice 1: parallel map. Same signature shape as
            // `map`; the runtime spawns OS threads (capped by
            // LEX_PAR_MAX_CONCURRENCY) to apply the closure
            // concurrently. Effect row stays open so a closure with
            // declared effects still type-checks against
            // par_map — though slice 1's runtime currently refuses
            // effectful closures at execution (queued as slice 2).
            fields.insert("par_map".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(7), Ty::Var(1)),
                ],
                EffectSet::open_var(7),
                Ty::List(Box::new(Ty::Var(1))),
            ));
            // #338: sort_by :: List[T], (T) -> [E] K -> [E] List[T]
            // Stable sort by the key the closure derives from each
            // element. K is intended to be one of Int / Float / Str
            // (the runtime comparator falls back to equality for
            // other shapes, preserving original order via the
            // stable sort) but the type system doesn't enforce that
            // — keep the signature minimal so callers can pass any
            // K and trust the comparator.
            fields.insert("sort_by".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(8), Ty::Var(1)),
                ],
                EffectSet::open_var(8),
                Ty::List(Box::new(Ty::Var(0))),
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
            // reverse :: List[T] -> List[T]
            fields.insert("reverse".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            // #334: cons :: T, List[T] -> List[T]  — O(1)-amortised prepend.
            fields.insert("cons".into(), Ty::function(
                vec![Ty::Var(0), Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            // enumerate :: List[T] -> List[(Int, T)]
            // Pairs each element with its zero-based index.
            fields.insert("enumerate".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Tuple(vec![Ty::int(), Ty::Var(0)]))),
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
            // now_ms :: () -> [time] Int — unix milliseconds (#378).
            // Resolution beyond what `time.now` (seconds) offers, for
            // request-latency measurement / rate-limiter windows.
            // Honors `LEX_TEST_NOW` for deterministic tests.
            fields.insert("now_ms".into(), Ty::function(
                vec![],
                EffectSet::singleton("time"),
                Ty::int(),
            ));
            // now_str :: () -> [time] Str — wall-clock instant rendered
            // as an ISO-8601 / RFC 3339 string in UTC (#378). Suitable
            // for auto-managed `created_at` / `updated_at` timestamps
            // and structured log lines. Honors `LEX_TEST_NOW`.
            fields.insert("now_str".into(), Ty::function(
                vec![],
                EffectSet::singleton("time"),
                Ty::str(),
            ));
            // mono_ns :: () -> [time] Int — monotonic-clock nanoseconds
            // since process start (#378). Use for *duration*
            // measurement (`end - start`); the value carries no wall-
            // clock meaning and the clock can never go backwards
            // (unlike `time.now_ms` under NTP jitter). Not affected by
            // `LEX_TEST_NOW` — pinning a monotonic clock would defeat
            // its purpose; tests that need a fake monotonic clock
            // should inject one through `EffectHandler`.
            fields.insert("mono_ns".into(), Ty::function(
                vec![],
                EffectSet::singleton("time"),
                Ty::int(),
            ));
            // sleep_ms :: Int -> [time] Unit (#226).
            // Used internally by flow.retry_with_backoff for
            // exponential-backoff delays; also available to user
            // code under `--allow-effects time`.
            fields.insert("sleep_ms".into(), Ty::function(
                vec![Ty::int()],
                EffectSet::singleton("time"),
                Ty::Unit,
            ));
            // sleep :: Duration -> [time] Unit (#445).
            // Duration-typed sleep — pairs with the
            // `datetime.duration_seconds` / `duration_minutes` /
            // `duration_days` constructors so periodic-task code
            // expresses the period in units of meaning rather than
            // raw milliseconds. Backed by `std::thread::sleep` at
            // runtime — blocks the calling thread, which is the right
            // semantics for the agent-driven workloads this exists
            // for. Inside a `net.serve` worker the same caveat as
            // `LEX_NET_INLINE_VM=1` applies (worker is stalled for `d`).
            fields.insert("sleep".into(), Ty::function(
                vec![Ty::Con("Duration".into(), vec![])],
                EffectSet::singleton("time"),
                Ty::Unit,
            ));
            Some(Ty::Record(fields))
        }
        "rand" => {
            // rand.int_in(lo, hi) -> [random] Int — honest uniform draw in
            // [lo, hi] (inclusive) from the OS RNG (#677). Carries the same
            // `[random]` effect as `crypto.random` rather than a separate
            // `rand` effect, so a reviewer auditing `--effect random` sees
            // every non-deterministic draw in one place. For deterministic,
            // replayable randomness thread a seed through `std.random`
            // instead; for cryptographic strength use `crypto.random`.
            let mut fields = IndexMap::new();
            fields.insert("int_in".into(), Ty::function(
                vec![Ty::int(), Ty::int()],
                EffectSet::singleton("random"),
                Ty::int(),
            ));
            Some(Ty::Record(fields))
        }
        "random" => {
            // #219: pure, seeded RNG. The caller threads the `Rng`
            // value through computations explicitly — there is no
            // global state and no effect tag, because the seed is
            // visible in the program's value flow and replay is
            // therefore deterministic by construction.
            //
            // Backed at runtime by SplitMix64 (deterministic across
            // platforms, single-u64 state). The proposal mentioned
            // `rand_chacha` for cryptographic-strength bias, but the
            // acceptance criterion is just "byte-identical sequence
            // across platforms," and SplitMix64 satisfies that with
            // a state shape that fits in `Value::Int` cleanly.
            let rng_t = || Ty::Con("Rng".into(), vec![]);
            let mut fields = IndexMap::new();
            // seed :: Int -> Rng
            fields.insert("seed".into(), Ty::function(
                vec![Ty::int()], EffectSet::empty(), rng_t()));
            // int :: Rng, Int, Int -> (Int, Rng)
            // Uniform in [lo, hi] inclusive at both ends. Returns
            // the drawn value and the advanced Rng.
            fields.insert("int".into(), Ty::function(
                vec![rng_t(), Ty::int(), Ty::int()],
                EffectSet::empty(),
                Ty::Tuple(vec![Ty::int(), rng_t()])));
            // float :: Rng -> (Float, Rng)
            // Uniform in [0.0, 1.0).
            fields.insert("float".into(), Ty::function(
                vec![rng_t()], EffectSet::empty(),
                Ty::Tuple(vec![Ty::float(), rng_t()])));
            // choose :: Rng, List[T] -> Option[(T, Rng)]
            // Returns None if the list is empty.
            fields.insert("choose".into(), Ty::function(
                vec![rng_t(), Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![
                    Ty::Tuple(vec![Ty::Var(0), rng_t()]),
                ]),
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
            // serve_ws_fn[Eff] :: (Int, Str, (WsConn, WsMessage) -> [Eff] WsAction)
            //                      -> [net, Eff] Unit
            // Effect-polymorphic WebSocket server that accepts a handler closure.
            // The second argument is the subprotocol string for the
            // Sec-WebSocket-Protocol handshake header ("" for none).
            // open_var(0) propagates the handler's effect row to the call site.
            fields.insert("serve_ws_fn".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::str(), // subprotocol
                    Ty::function(
                        vec![
                            Ty::Con("WsConn".into(), vec![]),
                            Ty::Con("WsMessage".into(), vec![]),
                        ],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));
            // serve_ws_fn_auth[Eff] :: (Int, Str,
            //   (Str, List[{name :: Str, value :: Str}]) -> [Eff] Result[Unit, Str],
            //   (WsConn, WsMessage) -> [Eff] WsAction)
            //   -> [net, Eff] Unit
            // Variant of serve_ws_fn that runs a pre-handshake auth
            // callback against the upgrade request's path + headers.
            // `Err(msg)` from the callback responds 401 Unauthorized
            // and skips the WS upgrade entirely (#423). The auth and
            // message-handler closures share the same effect row, so
            // a caller using e.g. `[sql]` to look up a password hash
            // in auth can use `[sql]` in subsequent handlers without
            // duplicating the declaration.
            let header_entry = || {
                let mut fs = IndexMap::new();
                fs.insert("name".into(),  Ty::str());
                fs.insert("value".into(), Ty::str());
                Ty::Record(fs)
            };
            fields.insert("serve_ws_fn_auth".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::str(), // subprotocol
                    // auth callback: (path, headers) -> [Eff] Result[Unit, Str]
                    Ty::function(
                        vec![
                            Ty::str(),
                            Ty::List(Box::new(header_entry())),
                        ],
                        EffectSet::open_var(0),
                        Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()]),
                    ),
                    // on_message: same shape as serve_ws_fn
                    Ty::function(
                        vec![
                            Ty::Con("WsConn".into(), vec![]),
                            Ty::Con("WsMessage".into(), vec![]),
                        ],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));
            // serve_ws_fn_actor[Eff] ::
            //   (Int, Str,
            //    (WsConn) -> Str,                        # name_of, registry name
            //    (WsConn, WsMessage) -> [Eff] WsAction)  # on_message
            //   -> [net, concurrent, Eff] Unit
            //
            // Variant of serve_ws_fn that registers each accepted
            // connection as a named actor in conc_registry. Non-WS
            // callers can then `conc.lookup(name) |> conc.tell(frame)`
            // to push outbound frames into the socket from arbitrary
            // [concurrent]-tagged code (HTTP webhooks, scheduled tasks,
            // broadcast loops). Documented in #459.
            //
            // name_of is intentionally pure: it inspects the WsConn
            // record (id / path / subprotocol) and decides what
            // name to register the connection under. Empty string
            // means "don't register this connection" — `on_message`
            // still runs but no outbound handle is exposed.
            //
            // The result row carries `concurrent` because the runtime
            // registers an `ActorHandler::Native` bridge in the conc
            // registry; lookups from non-WS callers are themselves
            // `[concurrent]` effects.
            fields.insert("serve_ws_fn_actor".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::str(), // subprotocol
                    Ty::function(
                        vec![Ty::Con("WsConn".into(), vec![])],
                        EffectSet::empty(),
                        Ty::str(),
                    ),
                    Ty::function(
                        vec![
                            Ty::Con("WsConn".into(), vec![]),
                            Ty::Con("WsMessage".into(), vec![]),
                        ],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0)
                    .union(&EffectSet::singleton("net"))
                    .union(&EffectSet::singleton("concurrent")),
                Ty::Unit,
            ));
            // dial_ws[Eff] :: (Str, Str, () -> [Eff] WsAction,
            //                  (WsMessage) -> [Eff] WsAction)
            //                  -> [net, Eff] Result[Unit, Str]
            //
            // WebSocket *client* — the inverse of serve_ws_fn (#390).
            // Connects to `url` (ws:// or wss://) with the given
            // subprotocol header, calls `on_open` once after the
            // handshake completes, then loops invoking `on_message`
            // for every inbound frame. Each callback returns a
            // `WsAction` that gets applied to the socket — same enum
            // as the server side, same semantics for `WsSend` /
            // `WsSendBinary` / `WsNoOp`. open_var(0) propagates the
            // handler effects so callers that touch [io], [time],
            // [random] etc. inside their handlers see those propagate
            // out of the dial_ws call.
            //
            // Returns `Result[Unit, Str]` rather than the bare `Unit`
            // that serve_ws_fn returns: a dial can fail on connect
            // (DNS, refused, bad TLS) or mid-stream (read error,
            // unexpected close) and the caller usually wants to know.
            fields.insert("dial_ws".into(), Ty::function(
                vec![
                    Ty::str(), // url (ws:// or wss://)
                    Ty::str(), // subprotocol (Sec-WebSocket-Protocol)
                    Ty::function(
                        vec![],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                    Ty::function(
                        vec![Ty::Con("WsMessage".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()]),
            ));
            // dial_ws_actor[Eff] :: (Str, Str, Str,
            //                        () -> [Eff] WsAction,
            //                        (WsMessage) -> [Eff] WsAction)
            //                        -> [net, Eff] Result[Unit, Str]
            //
            // Variant of dial_ws that registers the outgoing connection in the
            // conc registry under `name`. conc.tell(actor, frame_str) enqueues
            // a frame for delivery, enabling proactive sends (heartbeats,
            // meter values) from any other actor without changing the
            // reactive on_message signature.
            fields.insert("dial_ws_actor".into(), Ty::function(
                vec![
                    Ty::str(), // url
                    Ty::str(), // subprotocol ("" for none)
                    Ty::str(), // conc registry name ("" to skip registration)
                    Ty::function(
                        vec![],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                    Ty::function(
                        vec![Ty::Con("WsMessage".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("WsAction".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Con("Result".into(), vec![Ty::Unit, Ty::str()]),
            ));
            // serve_fn[Eff] :: (Int, (Request) -> [Eff] Response) -> [net, Eff] Unit
            // Effect-polymorphic variant of serve that accepts a first-class closure
            // instead of a handler name. open_var(0) captures the handler's effect row
            // so callers that invoke e.g. [io] effects inside the closure propagate them
            // to the serve_fn call site.
            fields.insert("serve_fn".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));
            // serve_routed[Eff] :: (
            //     Int,
            //     List[(Str, Str, (Request) -> [Eff] Response)],
            //     (Request) -> [Eff] Response
            //   ) -> [net, Eff] Unit
            //
            // Pattern-matched dispatch over `serve_fn`. Each route is a
            // (method, path-pattern, handler) triple — method is an
            // HTTP verb (case-insensitive) or "*" for any; path-patterns
            // use `:name` segments (e.g. "/users/:id") and matched values
            // are stamped onto `req.path_params` before the handler runs.
            // Routes are tried in registration order; the first match
            // wins. `fallback` runs when no route matches — typically a
            // 404 responder. Same `open_var(0)` effect-row trick as
            // `serve_fn` so handler effects propagate to the call site.
            fields.insert("serve_routed".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::List(Box::new(Ty::Tuple(vec![
                        Ty::str(),
                        Ty::str(),
                        Ty::function(
                            vec![Ty::Con("Request".into(), vec![])],
                            EffectSet::open_var(0),
                            Ty::Con("Response".into(), vec![]),
                        ),
                    ]))),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));

            // ServeOpts is a structural record literal — callers build
            // it with `{ http2: ..., inline_vm: ..., host: ... }`. Used
            // by `serve_with` / `serve_fn_with` / `serve_routed_with`
            // to replace the legacy LEX_NET_HTTP2 / LEX_NET_INLINE_VM
            // env-var gates with a first-class, type-checked config.
            // See lex-lang#497.
            let serve_opts_t = || {
                let mut fs = IndexMap::new();
                fs.insert("http2".into(),     Ty::bool());
                fs.insert("inline_vm".into(), Ty::bool());
                fs.insert("host".into(),      Ty::str());
                Ty::Record(fs)
            };

            // default_opts :: () -> ServeOpts
            // Returns the same defaults the legacy serve* paths use —
            // http2=false, inline_vm=false, host="0.0.0.0". Pure; the
            // env-var fallback only applies on the legacy serve* path,
            // not here.
            fields.insert("default_opts".into(), Ty::function(
                vec![],
                EffectSet::empty(),
                serve_opts_t(),
            ));

            // serve_with :: (Int, Str, ServeOpts) -> [net] Unit
            fields.insert("serve_with".into(), Ty::function(
                vec![Ty::int(), Ty::str(), serve_opts_t()],
                EffectSet::singleton("net"),
                Ty::Unit,
            ));

            // serve_fn_with[Eff] :: (Int, (Request) -> [Eff] Response, ServeOpts)
            //                       -> [net, Eff] Unit
            fields.insert("serve_fn_with".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                    serve_opts_t(),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));

            // serve_routed_with[Eff] :: (
            //   Int, List[(Str, Str, (Request) -> [Eff] Response)],
            //   (Request) -> [Eff] Response, ServeOpts
            // ) -> [net, Eff] Unit
            fields.insert("serve_routed_with".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::List(Box::new(Ty::Tuple(vec![
                        Ty::str(),
                        Ty::str(),
                        Ty::function(
                            vec![Ty::Con("Request".into(), vec![])],
                            EffectSet::open_var(0),
                            Ty::Con("Response".into(), vec![]),
                        ),
                    ]))),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                    serve_opts_t(),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));

            // serve_quic / serve_quic_fn / serve_quic_routed (#496).
            // HTTP/3 over QUIC. TlsConfig is an opaque value built by
            // `tls.from_pem_files` or `tls.self_signed` — it carries the
            // server certificate chain + private key needed for the
            // QUIC handshake (TLS is mandatory for HTTP/3). Effect row
            // stays `[net]` for symmetry with `serve` / `serve_fn`;
            // policy gates don't distinguish HTTP/1.1+2 (TCP) from
            // HTTP/3 (UDP) at the effect level.
            //
            // serve_quic :: (Int, TlsConfig, Str) -> [net] Unit
            fields.insert("serve_quic".into(), Ty::function(
                vec![Ty::int(), Ty::Con("TlsConfig".into(), vec![]), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit,
            ));

            // serve_quic_fn[Eff] :: (Int, TlsConfig,
            //                        (Request) -> [Eff] Response)
            //                       -> [net, Eff] Unit
            fields.insert("serve_quic_fn".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::Con("TlsConfig".into(), vec![]),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));

            // serve_quic_routed[Eff] :: (
            //   Int, TlsConfig,
            //   List[(Str, Str, (Request) -> [Eff] Response)],
            //   (Request) -> [Eff] Response
            // ) -> [net, Eff] Unit
            fields.insert("serve_quic_routed".into(), Ty::function(
                vec![
                    Ty::int(),
                    Ty::Con("TlsConfig".into(), vec![]),
                    Ty::List(Box::new(Ty::Tuple(vec![
                        Ty::str(),
                        Ty::str(),
                        Ty::function(
                            vec![Ty::Con("Request".into(), vec![])],
                            EffectSet::open_var(0),
                            Ty::Con("Response".into(), vec![]),
                        ),
                    ]))),
                    Ty::function(
                        vec![Ty::Con("Request".into(), vec![])],
                        EffectSet::open_var(0),
                        Ty::Con("Response".into(), vec![]),
                    ),
                ],
                EffectSet::open_var(0).union(&EffectSet::singleton("net")),
                Ty::Unit,
            ));

            Some(Ty::Record(fields))
        }
        // `tls` — TLS certificate handling for `net.serve_quic` (#496).
        // `TlsConfig` is opaque to user code; the only ways to obtain
        // one are these constructors. The runtime keeps the certificate
        // chain + private key behind that opaque type so we can change
        // the internal representation (record-of-bytes today, possibly
        // a Resource handle tomorrow) without breaking source code.
        "tls" => {
            let mut fields = IndexMap::new();
            // from_pem_files :: (Str, Str) -> [fs_read] Result[TlsConfig, Str]
            //                    cert  key
            // Load a PEM-encoded certificate chain + private key from
            // disk. Both paths are read with the `[fs_read]` effect so
            // policy gates can restrict where certs may come from.
            fields.insert("from_pem_files".into(), Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::singleton("fs_read"),
                Ty::Con("Result".into(), vec![
                    Ty::Con("TlsConfig".into(), vec![]),
                    Ty::str(),
                ]),
            ));
            // self_signed :: Str -> Result[TlsConfig, Str]
            // Generate a self-signed certificate for the given hostname
            // (or "localhost"). Pure — no effects needed. Intended for
            // local development and integration tests only; real
            // deployments should use a CA-signed cert via from_pem_files.
            fields.insert("self_signed".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![
                    Ty::Con("TlsConfig".into(), vec![]),
                    Ty::str(),
                ]),
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
        "conc" => {
            // Actor model (#381). Effect: [concurrent].
            // spawn :: S, (S, M) -> [E] (S, R) -> [concurrent] Actor[S]
            // ask   :: Actor[S], M -> [concurrent] R
            // tell  :: Actor[S], M -> [concurrent] Unit
            //
            // The type variables used here are fresh placeholders;
            // the checker instantiates them at each call site.
            //   0 = S (state), 1 = M (message), 2 = R (reply), 3 = E (effect row)
            let actor_t = |s: Ty| Ty::Con("Actor".into(), vec![s]);
            let mut fields = IndexMap::new();
            // spawn :: S, (S, M -> [E] (S, R)) -> [concurrent] Actor[S]
            fields.insert("spawn".into(), Ty::function(
                vec![
                    Ty::Var(0),
                    Ty::Function {
                        params: vec![Ty::Var(0), Ty::Var(1)],
                        effects: EffectSet::open_var(3),
                        ret: Box::new(Ty::Tuple(vec![Ty::Var(0), Ty::Var(2)])),
                    },
                ],
                EffectSet::singleton("concurrent"),
                actor_t(Ty::Var(0)),
            ));
            // ask :: Actor[S], M -> [concurrent] R
            fields.insert("ask".into(), Ty::function(
                vec![actor_t(Ty::Var(0)), Ty::Var(1)],
                EffectSet::singleton("concurrent"),
                Ty::Var(2),
            ));
            // tell :: Actor[S], M -> [concurrent] Unit
            fields.insert("tell".into(), Ty::function(
                vec![actor_t(Ty::Var(0)), Ty::Var(1)],
                EffectSet::singleton("concurrent"),
                Ty::Unit,
            ));
            // #444 — named-actor discovery within a process.
            //
            // register :: Actor[S], Str -> [concurrent] Result[Unit, ConcError]
            //   Returns Err(AlreadyRegistered(name)) if the name is
            //   taken — registration is exclusive so name collisions
            //   surface at the source level, not as silent overwrites.
            //
            // lookup :: Str -> [concurrent] Option[Actor[S]]
            //   Returns Some(actor) if registered, None otherwise. The
            //   static `[S]` parametrisation isn't checked at runtime in
            //   v1; the caller is responsible for matching the
            //   registration site's type. SigId-tagged variant deferred —
            //   see `conc_registry.rs` in lex-bytecode.
            //
            // unregister :: Str -> [concurrent] Result[Unit, ConcError]
            //   Returns Err(NotRegistered(name)) if absent. Existing
            //   `Actor[S]` handles held by callers continue to work
            //   after unregistration; the cell is reclaimed when the
            //   last handle drops.
            //
            // registered :: () -> [concurrent] List[Str]
            //   Sorted snapshot of currently registered names. Debug /
            //   introspection — not part of the steady-state agent flow.
            let conc_err = || Ty::Con("ConcError".into(), vec![]);
            let result_ce = |ok: Ty| Ty::Con("Result".into(), vec![ok, conc_err()]);
            fields.insert("register".into(), Ty::function(
                vec![actor_t(Ty::Var(0)), Ty::str()],
                EffectSet::singleton("concurrent"),
                result_ce(Ty::Unit),
            ));
            fields.insert("lookup".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("concurrent"),
                Ty::Con("Option".into(), vec![actor_t(Ty::Var(0))]),
            ));
            fields.insert("unregister".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("concurrent"),
                result_ce(Ty::Unit),
            ));
            fields.insert("registered".into(), Ty::function(
                vec![],
                EffectSet::singleton("concurrent"),
                Ty::List(Box::new(Ty::str())),
            ));
            Some(Ty::Record(fields))
        }
        "arrow" => {
            // Apache Arrow tables (#426). All ops are pure (no effects);
            // tables are immutable and conversions / reductions all run as
            // one Rust call over the flat buffer.
            //
            // `arrow.Table` is opaque from the type system's point of view —
            // the runtime variant `Value::ArrowTable` is the only producer
            // and consumer, so we model it as a 0-arity type constructor.
            let table = Ty::Con("Table".into(), vec![]);
            let str_t   = Ty::str();
            let int_t   = Ty::int();
            let float_t = Ty::float();
            let opt = |inner: Ty| Ty::Con("Option".into(), vec![inner]);
            let res = |ok: Ty| Ty::Con("Result".into(), vec![ok, Ty::str()]);
            let no_eff = EffectSet::empty();

            let mut fields = IndexMap::new();

            // -- constructors --
            // arrow.from_int_columns   :: List[(Str, List[Int])]   -> Result[Table, Str]
            // arrow.from_float_columns :: List[(Str, List[Float])] -> Result[Table, Str]
            // arrow.from_str_columns   :: List[(Str, List[Str])]   -> Result[Table, Str]
            for (name, elem) in [
                ("from_int_columns",   int_t.clone()),
                ("from_float_columns", float_t.clone()),
                ("from_str_columns",   str_t.clone()),
            ] {
                fields.insert(name.into(), Ty::function(
                    vec![Ty::List(Box::new(Ty::Tuple(vec![
                        str_t.clone(),
                        Ty::List(Box::new(elem)),
                    ])))],
                    no_eff.clone(),
                    res(table.clone()),
                ));
            }

            // -- introspection --
            // arrow.nrows / arrow.ncols :: Table -> Int
            fields.insert("nrows".into(), Ty::function(
                vec![table.clone()], no_eff.clone(), int_t.clone()));
            fields.insert("ncols".into(), Ty::function(
                vec![table.clone()], no_eff.clone(), int_t.clone()));
            // arrow.col_names :: Table -> List[Str]
            fields.insert("col_names".into(), Ty::function(
                vec![table.clone()], no_eff.clone(),
                Ty::List(Box::new(str_t.clone()))));
            // arrow.col_type :: Table, Str -> Option[Str]
            fields.insert("col_type".into(), Ty::function(
                vec![table.clone(), str_t.clone()],
                no_eff.clone(), opt(str_t.clone())));

            // -- column reductions --
            // arrow.col_sum_int   :: Table, Str -> Result[Int, Str]
            // arrow.col_sum_float :: Table, Str -> Result[Float, Str]
            // arrow.col_mean      :: Table, Str -> Result[Option[Float], Str]
            // arrow.col_min_int   :: Table, Str -> Result[Option[Int], Str]
            // arrow.col_max_int   :: Table, Str -> Result[Option[Int], Str]
            // arrow.col_count     :: Table, Str -> Result[Int, Str]
            for (name, ret_ok) in [
                ("col_sum_int",   int_t.clone()),
                ("col_sum_float", float_t.clone()),
                ("col_mean",      opt(float_t.clone())),
                ("col_min_int",   opt(int_t.clone())),
                ("col_max_int",   opt(int_t.clone())),
                ("col_count",     int_t.clone()),
            ] {
                fields.insert(name.into(), Ty::function(
                    vec![table.clone(), str_t.clone()],
                    no_eff.clone(), res(ret_ok)));
            }

            // -- slicing --
            // arrow.head / tail :: Table, Int -> Table
            for name in &["head", "tail"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), int_t.clone()],
                    no_eff.clone(), table.clone()));
            }
            // arrow.slice :: Table, Int, Int -> Table
            fields.insert("slice".into(), Ty::function(
                vec![table.clone(), int_t.clone(), int_t.clone()],
                no_eff.clone(), table.clone()));
            // arrow.select_cols :: Table, List[Str] -> Result[Table, Str]
            fields.insert("select_cols".into(), Ty::function(
                vec![table.clone(), Ty::List(Box::new(str_t.clone()))],
                no_eff.clone(), res(table.clone())));
            // arrow.drop_col :: Table, Str -> Result[Table, Str]
            fields.insert("drop_col".into(), Ty::function(
                vec![table.clone(), str_t.clone()],
                no_eff.clone(), res(table.clone())));

            // -- I/O (effect-gated) --
            // arrow.read_csv :: Str -> [fs_read] Result[Table, Str]
            // Header row required; schema inferred from the first 100 rows.
            // The `[fs_read]` effect surfaces in agent-tool policy gates
            // and `--allow-fs-read` per-path scoping, same as `io.read`.
            fields.insert("read_csv".into(), Ty::function(
                vec![str_t.clone()],
                EffectSet::singleton("fs_read"),
                res(table.clone())));

            // arrow.read_parquet :: Str -> [fs_read] Result[Table, Str]
            // arrow.read_parquet_cols :: (Str, List[Str]) -> [fs_read] Result[Table, Str]
            // Same effect + path-scope rules as read_csv. _cols pushes
            // the projection into the Parquet reader (no decode of skipped
            // columns); missing column names surface as Err.
            fields.insert("read_parquet".into(), Ty::function(
                vec![str_t.clone()],
                EffectSet::singleton("fs_read"),
                res(table.clone())));
            fields.insert("read_parquet_cols".into(), Ty::function(
                vec![str_t.clone(), Ty::List(Box::new(str_t.clone()))],
                EffectSet::singleton("fs_read"),
                res(table.clone())));

            // arrow.write_parquet :: (Table, Str) -> [fs_write] Result[Unit, Str]
            // arrow.write_csv     :: (Table, Str) -> [fs_write] Result[Unit, Str]
            // Path scope uses --allow-fs-write (symmetric with io.write).
            // Parquet default: Snappy compression, default page/row-group
            // sizes — sufficient for v1; a write_parquet_opts variant
            // can ride a later issue if knobs are needed.
            for name in &["write_parquet", "write_csv"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), str_t.clone()],
                    EffectSet::singleton("fs_write"),
                    res(Ty::Unit)));
            }

            Some(Ty::Record(fields))
        }
        "df" => {
            // Polars-backed query ops over arrow.Table (#427). All pure
            // (no effects); the Polars DataFrame is internal plumbing,
            // never leaves the kernel.
            let table = Ty::Con("Table".into(), vec![]);
            let str_t = Ty::str();
            let int_t = Ty::int();
            let float_t = Ty::float();
            let bool_t = Ty::bool();
            let res = |ok: Ty| Ty::Con("Result".into(), vec![ok, Ty::str()]);
            let no_eff = EffectSet::empty();

            let mut fields = IndexMap::new();

            // df.filter_{eq,gt,lt}_int :: Table, Str, Int -> Result[Table, Str]
            for name in &["filter_eq_int", "filter_gt_int", "filter_lt_int"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), str_t.clone(), int_t.clone()],
                    no_eff.clone(), res(table.clone())));
            }

            // #433 — string filters.
            // df.filter_eq_str  :: Table, Str, Str       -> Result[Table, Str]
            // df.filter_in_str  :: Table, Str, List[Str] -> Result[Table, Str]
            fields.insert("filter_eq_str".into(), Ty::function(
                vec![table.clone(), str_t.clone(), str_t.clone()],
                no_eff.clone(), res(table.clone())));
            fields.insert("filter_in_str".into(), Ty::function(
                vec![table.clone(), str_t.clone(), Ty::List(Box::new(str_t.clone()))],
                no_eff.clone(), res(table.clone())));

            // #433 — float filters.
            // df.filter_{eq,lt,gt}_float :: Table, Str, Float -> Result[Table, Str]
            for name in &["filter_eq_float", "filter_lt_float", "filter_gt_float"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), str_t.clone(), float_t.clone()],
                    no_eff.clone(), res(table.clone())));
            }

            // #433 — null handling.
            // df.filter_isnull  :: Table, Str       -> Result[Table, Str]
            // df.filter_notnull :: Table, Str       -> Result[Table, Str]
            // df.drop_nulls     :: Table, List[Str] -> Result[Table, Str]
            for name in &["filter_isnull", "filter_notnull"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), str_t.clone()],
                    no_eff.clone(), res(table.clone())));
            }
            fields.insert("drop_nulls".into(), Ty::function(
                vec![table.clone(), Ty::List(Box::new(str_t.clone()))],
                no_eff.clone(), res(table.clone())));

            // df.sort_by :: Table, Str, Bool -> Result[Table, Str]
            fields.insert("sort_by".into(), Ty::function(
                vec![table.clone(), str_t.clone(), bool_t.clone()],
                no_eff.clone(), res(table.clone())));

            // df.group_by_agg :: Table, List[Str], List[(Str, Str, Str)]
            //                    -> Result[Table, Str]
            // Spec tuple is (out_col, in_col, op). op ∈
            // "sum"|"mean"|"min"|"max"|"count"|"n_distinct".
            fields.insert("group_by_agg".into(), Ty::function(
                vec![
                    table.clone(),
                    Ty::List(Box::new(str_t.clone())),
                    Ty::List(Box::new(Ty::Tuple(vec![
                        str_t.clone(), str_t.clone(), str_t.clone(),
                    ]))),
                ],
                no_eff.clone(), res(table.clone())));

            // df.inner_join / left_join :: Table, Table, Str -> Result[Table, Str]
            for name in &["inner_join", "left_join"] {
                fields.insert((*name).into(), Ty::function(
                    vec![table.clone(), table.clone(), str_t.clone()],
                    no_eff.clone(), res(table.clone())));
            }

            Some(Ty::Record(fields))
        }
        // `std.proc` was removed in favour of `std.process` (#678): its
        // single op `proc.spawn(cmd, args)` was byte-for-byte equivalent to
        // `process.run(cmd, args)` — same `[proc]` effect, same
        // `{ stdout, stderr, exit_code }` result — and `std.process` is a
        // strict superset (streaming spawn / read / wait / kill). Callers
        // migrate `proc.spawn` → `process.run`.
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
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::empty(),
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
            // result.unwrap_or :: Result[T, E], T -> T
            // Eager fallback — the Ok payload, or the supplied default on
            // Err. Mirrors option.unwrap_or (#679).
            fields.insert("unwrap_or".into(), Ty::function(
                vec![Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]), Ty::Var(0)],
                EffectSet::empty(),
                Ty::Var(0),
            ));
            // result.unwrap_or_else :: Result[T, E], (E) -> [Eff] T -> [Eff] T
            // Lazy fallback — the closure runs only on Err and receives the
            // error payload (effect-polymorphic on the closure). Mirrors
            // option.unwrap_or_else (#679).
            fields.insert("unwrap_or_else".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(1)], EffectSet::open_var(7), Ty::Var(0)),
                ],
                EffectSet::open_var(7),
                Ty::Var(0),
            ));
            // result.is_ok / is_err :: Result[T, E] -> Bool (#679)
            fields.insert("is_ok".into(), Ty::function(
                vec![Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)])],
                EffectSet::empty(), Ty::bool()));
            fields.insert("is_err".into(), Ty::function(
                vec![Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)])],
                EffectSet::empty(), Ty::bool()));
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
            // option.unwrap_or_else :: Option[T], () -> [E] T -> [E] T
            // Lazy variant of unwrap_or: the default is computed by a closure
            // only when the value is None (effect-polymorphic on the closure).
            fields.insert("unwrap_or_else".into(), Ty::function(
                vec![
                    Ty::Con("Option".into(), vec![Ty::Var(0)]),
                    Ty::function(vec![], EffectSet::open_var(5), Ty::Var(0)),
                ],
                EffectSet::open_var(5),
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
            // option.is_some / is_none :: Option[T] -> Bool (#679)
            fields.insert("is_some".into(), Ty::function(
                vec![Ty::Con("Option".into(), vec![Ty::Var(0)])],
                EffectSet::empty(), Ty::bool()));
            fields.insert("is_none".into(), Ty::function(
                vec![Ty::Con("Option".into(), vec![Ty::Var(0)])],
                EffectSet::empty(), Ty::bool()));
            // option.ok_or :: Option[T], E -> Result[T, E]
            // Cross from Option into Result, supplying the error for None (#679).
            fields.insert("ok_or".into(), Ty::function(
                vec![Ty::Con("Option".into(), vec![Ty::Var(0)]), Ty::Var(1)],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
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
        "iter" => {
            // Positional iterator (#364) + lazy variant via `iter.unfold`
            // (#376). Internal value shapes are `__IterEager(list, idx)` or
            // `__IterLazy(seed, step)`; all operations compile-inline and
            // dispatch on the variant tag at runtime.
            // Type var slots: 0 = T (element), 1 = U (mapped element) /
            // A (fold acc), 2 = S (unfold seed).
            let it = |n: u32| Ty::Con("Iter".into(), vec![Ty::Var(n)]);
            let mut fields = IndexMap::new();
            // from_list :: List[T] -> Iter[T]
            fields.insert("from_list".into(), Ty::function(
                vec![Ty::List(Box::new(Ty::Var(0)))],
                EffectSet::empty(), it(0)));
            // unfold[S, T] :: S, (S) -> Option[(T, S)] -> Iter[T] (#376)
            // The step closure may carry any effect row; the iterator
            // itself stays effect-free since the effects only fire when
            // the step is invoked via `iter.next` / `iter.to_list`.
            fields.insert("unfold".into(), Ty::function(
                vec![
                    Ty::Var(2), // seed S
                    Ty::function(
                        vec![Ty::Var(2)],
                        EffectSet::open_var(3),
                        Ty::Con("Option".into(), vec![
                            Ty::Tuple(vec![Ty::Var(0), Ty::Var(2)])
                        ]),
                    ),
                ],
                EffectSet::empty(), it(0)));
            // next :: Iter[T] -> Option[(T, Iter[T])]
            fields.insert("next".into(), Ty::function(
                vec![it(0)],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![
                    Ty::Tuple(vec![Ty::Var(0), it(0)])
                ])));
            // is_empty :: Iter[T] -> Bool
            fields.insert("is_empty".into(), Ty::function(
                vec![it(0)], EffectSet::empty(), Ty::bool()));
            // count :: Iter[T] -> Int   (remaining elements)
            fields.insert("count".into(), Ty::function(
                vec![it(0)], EffectSet::empty(), Ty::int()));
            // take :: Iter[T], Int -> Iter[T]
            fields.insert("take".into(), Ty::function(
                vec![it(0), Ty::int()], EffectSet::empty(), it(0)));
            // skip :: Iter[T], Int -> Iter[T]
            fields.insert("skip".into(), Ty::function(
                vec![it(0), Ty::int()], EffectSet::empty(), it(0)));
            // to_list :: Iter[T] -> List[T]
            fields.insert("to_list".into(), Ty::function(
                vec![it(0)], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0)))));
            // collect :: Iter[T] -> List[T] — alias for `to_list`
            // (matches Rust / Python / Kotlin naming so call sites
            // coming from those languages don't have to re-learn).
            fields.insert("collect".into(), Ty::function(
                vec![it(0)], EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0)))));
            // map :: [E] Iter[T], (T) -> [E] U -> [E] Iter[U]
            fields.insert("map".into(), Ty::function(
                vec![
                    it(0),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(2), Ty::Var(1)),
                ],
                EffectSet::open_var(2), it(1)));
            // filter :: [E] Iter[T], (T) -> [E] Bool -> [E] Iter[T]
            fields.insert("filter".into(), Ty::function(
                vec![
                    it(0),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(1), Ty::bool()),
                ],
                EffectSet::open_var(1), it(0)));
            // fold :: [E] Iter[T], A, (A, T) -> [E] A -> [E] A
            fields.insert("fold".into(), Ty::function(
                vec![
                    it(0),
                    Ty::Var(1),
                    Ty::function(vec![Ty::Var(1), Ty::Var(0)], EffectSet::open_var(2), Ty::Var(1)),
                ],
                EffectSet::open_var(2), Ty::Var(1)));
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
            // retry[T, U, E, Eff](
            //   f: (T) -> [Eff] Result[U, E], n: Int
            // ) -> (T) -> [Eff] Result[U, E]
            // open_var(3) is the effect row carried by `f`; the
            // combinator itself is pure, so the outer EffectSet is
            // empty. The returned closure propagates Eff unchanged.
            let result_ty = Ty::Con("Result".into(), vec![Ty::Var(1), Ty::Var(2)]);
            fields.insert("retry".into(), Ty::function(
                vec![
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3), result_ty.clone()),
                    Ty::int(),
                ],
                EffectSet::empty(),
                Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3), result_ty.clone()),
            ));
            // retry_with_backoff[T, U, E, Eff](
            //   f: (T) -> [Eff] Result[U, E], attempts: Int, base_ms: Int,
            // ) -> (T) -> [Eff, time] Result[U, E]
            // Same retry shape as `flow.retry` plus an exponential
            // backoff between attempts. The result function carries
            // `[time]` (from `time.sleep_ms`) unioned with the inner
            // closure's effect row Eff, so e.g. a `[net]` closure
            // produces a `[net, time]` result function. (#226)
            fields.insert("retry_with_backoff".into(), Ty::function(
                vec![
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3), result_ty.clone()),
                    Ty::int(),
                    Ty::int(),
                ],
                EffectSet::empty(),
                Ty::function(vec![Ty::Var(0)],
                    EffectSet::open_var(3).union(&EffectSet::singleton("time")), result_ty),
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
            // Hashes: Bytes -> Bytes (digest as raw bytes).
            // SHA-256 / SHA-512 are vetted. MD5 is retained only for
            // interop with legacy systems — new code should not use it.
            // BLAKE2b (#382) is included as a faster alternative to
            // SHA-512 with the same security level.
            for name in &["sha256", "sha512", "md5", "blake2b"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::bytes()],
                    EffectSet::empty(),
                    Ty::bytes(),
                ));
            }
            // Hex-string convenience hashers (#382): hash a Str directly,
            // return the digest as a lowercase hex Str. Equivalent to
            // `crypto.hex_encode(crypto.shaN(bytes_from_str(s)))` but
            // saves the two-step incantation for the common case.
            for name in &["sha256_str", "sha512_str"] {
                fields.insert((*name).into(), Ty::function(
                    vec![Ty::str()],
                    EffectSet::empty(),
                    Ty::str(),
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
            // ed25519 asymmetric signatures (#643). A secret key is its 32-byte
            // seed (generate via `crypto.random(32)`); all three ops are pure.
            //   ed25519_public_key(secret :: Bytes) -> Result[Bytes, Str]
            //   ed25519_sign(secret :: Bytes, message :: Bytes) -> Result[Bytes, Str]
            //   ed25519_verify(public :: Bytes, message :: Bytes, sig :: Bytes) -> Bool
            fields.insert("ed25519_public_key".into(), Ty::function(
                vec![Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("ed25519_sign".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("ed25519_verify".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::bool(),
            ));
            // P-256 ECDSA / ES256 (#651). The JWT/SD-JWT signature
            // algorithm for AP2 agent keys; `lex-jose` builds the
            // token layer on top of these primitives. Key bytes are
            // raw: a secret key is the 32-byte scalar, a public key is
            // the 33-byte SEC1 compressed point; JWK serialization
            // lives downstream in `lex-jose`.
            //
            //   p256_generate()                       -> [random] Result[Bytes, Str]
            //   p256_public_key(sk :: Bytes)          -> Result[Bytes, Str]
            //   p256_sign(sk :: Bytes, msg :: Bytes)  -> Result[Bytes, Str]
            //   p256_verify(pk :: Bytes, msg :: Bytes, sig :: Bytes) -> Bool
            //
            // `p256_generate` mints fresh key material from the OS RNG,
            // so it carries the same fine-grained `[random]` effect as
            // `crypto.random` — every key-minting call stays visible to
            // `lex audit --effect random`. (The issue sketched `[env]`;
            // `[random]` is the dedicated effect for OS randomness in
            // this codebase, so we use that for consistency.)
            // `sign`/`verify` are pure: signing hashes `msg` with
            // SHA-256 internally (standard ES256) and the signature is
            // DER-encoded.
            fields.insert("p256_generate".into(), Ty::function(
                vec![],
                EffectSet::singleton("random"),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("p256_public_key".into(), Ty::function(
                vec![Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("p256_sign".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("p256_verify".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::bool(),
            ));
            // secp256k1 ECDSA + recovery (#655). The EVM curve — backs
            // EIP-712 typed-data signing (EIP-3009 / x402 `exact`) and
            // Ethereum address derivation. Unlike `p256_*`/`ed25519_*`,
            // the sign/verify ops here take a **pre-hashed 32-byte
            // digest** (EIP-712 already hashes), hence the `_digest`
            // suffix — they do NOT hash the input again.
            //
            // - Secret key: 32-byte scalar.
            // - Public key: 65-byte UNCOMPRESSED SEC1 point (0x04‖X‖Y),
            //   so an address is `keccak256(pk[1..])[12..]` with no
            //   decompression step. (p256 returns compressed; the EVM
            //   convention is uncompressed.)
            // - Signature: 65 bytes `r(32)‖s(32)‖v(1)`, v ∈ {27,28}
            //   (Ethereum), low-S normalized (EIP-2).
            //
            //   keccak256(data :: Bytes) -> Bytes
            //   secp256k1_generate()                          -> [random] Result[Bytes, Str]
            //   secp256k1_public_key(sk :: Bytes)             -> Result[Bytes, Str]
            //   secp256k1_sign_digest(sk :: Bytes, digest :: Bytes) -> Result[Bytes, Str]
            //   secp256k1_recover(digest :: Bytes, sig :: Bytes)    -> Result[Bytes, Str]
            //   secp256k1_verify(pk :: Bytes, digest :: Bytes, sig :: Bytes) -> Bool
            //
            // `secp256k1_generate` mints from the OS RNG, so it carries
            // the same `[random]` effect as `crypto.random` / `p256_generate`.
            fields.insert("keccak256".into(), Ty::function(
                vec![Ty::bytes()],
                EffectSet::empty(),
                Ty::bytes(),
            ));
            fields.insert("secp256k1_generate".into(), Ty::function(
                vec![],
                EffectSet::singleton("random"),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("secp256k1_public_key".into(), Ty::function(
                vec![Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("secp256k1_sign_digest".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("secp256k1_recover".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("secp256k1_verify".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes(), Ty::bytes()],
                EffectSet::empty(),
                Ty::bool(),
            ));
            // base64 / hex
            fields.insert("base64_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("base64_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            // URL-safe base64 (#382): the alphabet swaps `+/` for `-_`
            // and omits padding. Required by JWT, signed-cookie, and
            // most token-bearing URL paths.
            fields.insert("base64url_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("base64url_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            fields.insert("hex_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("hex_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            // base58 (#658) — Bitcoin/Solana alphabet, no checksum. Solana
            // addresses, mints, signatures and the x402 `exact` payload are
            // base58; this is the Solana analog of keccak/secp256k1 (#655).
            fields.insert("base58_encode".into(), Ty::function(
                vec![Ty::bytes()], EffectSet::empty(), Ty::str()));
            fields.insert("base58_decode".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()])));
            // Constant-time equality (for HMAC verification etc.).
            // `eq` / `eq_str` (#382) are the recommended spelling;
            // `constant_time_eq` stays as a deprecated alias.
            fields.insert("constant_time_eq".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()], EffectSet::empty(), Ty::bool()));
            fields.insert("eq".into(), Ty::function(
                vec![Ty::bytes(), Ty::bytes()], EffectSet::empty(), Ty::bool()));
            fields.insert("eq_str".into(), Ty::function(
                vec![Ty::str(), Ty::str()], EffectSet::empty(), Ty::bool()));
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
            // random_str_hex (#382): the most common token-mint pattern
            // — N random bytes rendered as 2N lowercase hex chars.
            // Suitable for session ids, request ids, OAuth `state`,
            // CSRF tokens; not suitable as a JWT signing key (use raw
            // `random` for that).
            fields.insert("random_str_hex".into(), Ty::function(
                vec![Ty::int()],
                EffectSet::singleton("random"),
                Ty::str(),
            ));

            // AEAD: authenticated encryption with associated data
            // (#382 AEAD slice). Both algorithms use a 12-byte nonce
            // and a 16-byte authentication tag. `seal` returns the
            // structured `AeadResult { ciphertext, tag }`; `open`
            // returns `Result[Bytes, Str]` so authentication failures
            // surface as `Err`, not a panic.
            //
            // - **AES-GCM** (`aes_gcm_seal/open`): AES-128/192/256-GCM,
            //   key length determined by the supplied key bytes (16, 24,
            //   or 32). NIST-recommended; hardware-accelerated on most CPUs.
            // - **ChaCha20-Poly1305** (`chacha20_poly1305_seal/open`):
            //   Always a 32-byte key. Equivalent security to AES-GCM
            //   without needing AES-NI hardware; preferred on constrained
            //   targets.
            let aead_t = || Ty::Con("AeadResult".into(), vec![]);
            // Seal: returns Result[AeadResult, Str] rather than bare
            // AeadResult so input-validation errors (wrong key length,
            // wrong nonce length) surface as `Err` to the Lex caller
            // instead of panicking the VM. AES-GCM expects 16/24/32-byte
            // keys; ChaCha20-Poly1305 expects exactly 32. Both expect a
            // 12-byte nonce.
            for name in &["aes_gcm_seal", "chacha20_poly1305_seal"] {
                fields.insert((*name).into(), Ty::function(
                    // (key, nonce, aad, plaintext) -> Result[AeadResult, Str]
                    vec![Ty::bytes(), Ty::bytes(), Ty::bytes(), Ty::bytes()],
                    EffectSet::empty(),
                    Ty::Con("Result".into(), vec![aead_t(), Ty::str()]),
                ));
            }
            for name in &["aes_gcm_open", "chacha20_poly1305_open"] {
                fields.insert((*name).into(), Ty::function(
                    // (key, nonce, aad, ciphertext, tag) -> Result[Bytes, Str]
                    vec![Ty::bytes(), Ty::bytes(), Ty::bytes(), Ty::bytes(), Ty::bytes()],
                    EffectSet::empty(),
                    Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
                ));
            }

            // KDFs: key-derivation functions (#382 KDF slice). All three
            // return `Result[Bytes, Str]` so caller-controlled inputs
            // (iteration count, output length, argon2id work factors)
            // that violate the underlying primitive's contract surface
            // as `Err` rather than panicking the VM. None require a new
            // effect — these are pure derivations.
            //
            // - **`pbkdf2_sha256(password, salt, iterations, len)`** —
            //   RFC 8018 PBKDF2 with HMAC-SHA256. Use ≥ 600_000 iterations
            //   for password storage (OWASP 2024). Older deployments
            //   pinning < 100_000 should rotate.
            // - **`hkdf_sha256(ikm, salt, info, len)`** — RFC 5869 extract+
            //   expand. Use for deriving multiple keys from a single
            //   high-entropy input (TLS, Noise, JWT-key rotation).
            //   Output length capped at 255 × 32 = 8160 bytes.
            // - **`argon2id(password, salt, t_cost, m_cost, len)`** —
            //   RFC 9106 Argon2id. Recommended for *new* password
            //   hashing. OWASP 2024 baseline: `t_cost=2, m_cost=19456`
            //   (19 MiB), or use `lex-crypto`'s vetted wrapper.
            fields.insert("pbkdf2_sha256".into(), Ty::function(
                // (password, salt, iterations, len) -> Result[Bytes, Str]
                vec![Ty::bytes(), Ty::bytes(), Ty::int(), Ty::int()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("hkdf_sha256".into(), Ty::function(
                // (ikm, salt, info, len) -> Result[Bytes, Str]
                vec![Ty::bytes(), Ty::bytes(), Ty::bytes(), Ty::int()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
            ));
            fields.insert("argon2id".into(), Ty::function(
                // (password, salt, t_cost, m_cost, len) -> Result[Bytes, Str]
                vec![Ty::bytes(), Ty::bytes(), Ty::int(), Ty::int(), Ty::int()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::bytes(), Ty::str()]),
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
                    concrete: [crate::types::EffectKind::bare("io"), crate::types::EffectKind::bare("fs_write")].into_iter().collect(),
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
            // #331: comparison ops on Instant.
            fields.insert("before".into(), Ty::function(
                vec![inst(), inst()], EffectSet::empty(), Ty::bool()));
            fields.insert("after".into(), Ty::function(
                vec![inst(), inst()], EffectSet::empty(), Ty::bool()));
            // compare :: Instant, Instant -> Int  (-1 / 0 / +1)
            fields.insert("compare".into(), Ty::function(
                vec![inst(), inst()], EffectSet::empty(), Ty::int()));
            Some(Ty::Record(fields))
        }
        // #331: duration module — scalar extraction from Duration values.
        "duration" => {
            let dur = || Ty::Con("Duration".into(), vec![]);
            let mut fields = IndexMap::new();
            // Scalar extraction from a Duration (nanoseconds under the
            // hood). Each truncates toward zero. `seconds` shipped with
            // #331; #681 rounds out the unit set so a Duration built in
            // days via `datetime.duration_days` can be read back in the
            // same units rather than only as seconds.
            // millis / seconds / minutes / hours / days :: Duration -> Int
            for name in &["millis", "seconds", "minutes", "hours", "days"] {
                fields.insert((*name).into(), Ty::function(
                    vec![dur()], EffectSet::empty(), Ty::int()));
            }
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
                    concrete: [crate::types::EffectKind::bare("fs_walk"), crate::types::EffectKind::bare("fs_write")].into_iter().collect(),
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
                    concrete: [crate::types::EffectKind::bare("kv"), crate::types::EffectKind::bare("fs_write")].into_iter().collect(),
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
            // Embedded SQL (SQLite via rusqlite). The opaque `Db` type is
            // backed by an Int handle into a process-wide registry (#362).
            //
            // Params use the typed `SqlParam` ADT (PStr|PInt|PFloat|PBool|PNull)
            // registered in env.rs, so callers don't have to stringify values.
            //
            // Transactions: sql.begin(db) → SqlTx; sql.commit/rollback(tx).
            // exec_tx / query_tx mirror exec / query but operate on a SqlTx.
            //
            // Row decoders: get_str / get_int / get_float / get_bool extract
            // typed columns from a row record by name.
            let db_t  = || Ty::Con("Db".into(), vec![]);
            let tx_t  = || Ty::Con("SqlTx".into(), vec![]);
            let sp_t  = || Ty::Con("SqlParam".into(), vec![]);
            let params_t = || Ty::List(Box::new(sp_t()));
            let mut fields = IndexMap::new();

            // SqlError = { message, code, detail } — populated with
            // SQLSTATE (Postgres) or symbolic SQLite error name (#380).
            let se_t = || Ty::Con("SqlError".into(), vec![]);

            // open :: Str -> [sql, fs_write] Result[Db, SqlError]
            fields.insert("open".into(), Ty::function(
                vec![Ty::str()],
                EffectSet {
                    concrete: [crate::types::EffectKind::bare("sql"),
                               crate::types::EffectKind::bare("fs_write")]
                        .into_iter().collect(),
                    var: None,
                },
                Ty::Con("Result".into(), vec![db_t(), se_t()])));

            // close :: Db -> [sql] Unit
            fields.insert("close".into(), Ty::function(
                vec![db_t()],
                EffectSet::singleton("sql"),
                Ty::Unit));

            // exec :: Db, Str, List[SqlParam] -> [sql] Result[Int, SqlError]
            fields.insert("exec".into(), Ty::function(
                vec![db_t(), Ty::str(), params_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![Ty::int(), se_t()])));

            // query[T] :: Db, Str, List[SqlParam] -> [sql] Result[List[T], SqlError]
            fields.insert("query".into(), Ty::function(
                vec![db_t(), Ty::str(), params_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    se_t(),
                ])));

            // query_iter[T] :: Db, Str, List[SqlParam] -> [sql] Result[Iter[T], SqlError]
            // Streaming variant of `query` (#379). Rows are pulled from
            // the server one at a time via an mpsc-backed cursor —
            // memory stays bounded regardless of result-set size.
            // Other ops on the same `Db` handle block until the cursor
            // is drained (single connection per Db).
            fields.insert("query_iter".into(), Ty::function(
                vec![db_t(), Ty::str(), params_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![
                    Ty::Con("Iter".into(), vec![Ty::Var(0)]),
                    se_t(),
                ])));

            // begin :: Db -> [sql] Result[SqlTx, SqlError]
            fields.insert("begin".into(), Ty::function(
                vec![db_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![tx_t(), se_t()])));

            // commit :: SqlTx -> [sql] Result[Unit, SqlError]
            fields.insert("commit".into(), Ty::function(
                vec![tx_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![Ty::Unit, se_t()])));

            // rollback :: SqlTx -> [sql] Result[Unit, SqlError]
            fields.insert("rollback".into(), Ty::function(
                vec![tx_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![Ty::Unit, se_t()])));

            // exec_tx :: SqlTx, Str, List[SqlParam] -> [sql] Result[Int, SqlError]
            fields.insert("exec_tx".into(), Ty::function(
                vec![tx_t(), Ty::str(), params_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![Ty::int(), se_t()])));

            // query_tx[T] :: SqlTx, Str, List[SqlParam] -> [sql] Result[List[T], SqlError]
            fields.insert("query_tx".into(), Ty::function(
                vec![tx_t(), Ty::str(), params_t()],
                EffectSet::singleton("sql"),
                Ty::Con("Result".into(), vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    se_t(),
                ])));

            // Row decoders: get_X[T] :: T, Str -> Option[X]
            // T is polymorphic so these work on any row record shape.
            fields.insert("get_str".into(), Ty::function(
                vec![Ty::Var(0), Ty::str()],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::str()])));
            fields.insert("get_int".into(), Ty::function(
                vec![Ty::Var(0), Ty::str()],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::int()])));
            fields.insert("get_float".into(), Ty::function(
                vec![Ty::Var(0), Ty::str()],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::float()])));
            fields.insert("get_bool".into(), Ty::function(
                vec![Ty::Var(0), Ty::str()],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::bool()])));

            Some(Ty::Record(fields))
        }
        "redis" => {
            // Thin Redis client (#533). ConnRedis is an opaque handle backed by a
            // process-wide registry (same pattern as Db in std.sql). All ops carry
            // [net] — Redis is a TCP service; no separate [redis] effect.
            //
            // subscribe / psubscribe return Nil (= Unit) because they are blocking
            // infinite loops, consistent with net.serve_fn and ws.serve.
            //
            // subscribe/psubscribe open a *dedicated* connection internally —
            // Redis disallows non-Pub/Sub commands on a subscribed connection.
            let conn_t = || Ty::Con("ConnRedis".into(), vec![]);
            let mut fields = IndexMap::new();

            // connect :: Str -> [net] Result[ConnRedis, Str]
            // url: "redis://host:6379" or "rediss://host:6380" (TLS)
            fields.insert("connect".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("net"),
                Ty::Con("Result".into(), vec![conn_t(), Ty::str()])));

            // close :: ConnRedis -> [net] Unit
            fields.insert("close".into(), Ty::function(
                vec![conn_t()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // ---- Key-value -----------------------------------------------

            // get :: ConnRedis, Str -> [net] Option[Str]
            fields.insert("get".into(), Ty::function(
                vec![conn_t(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Con("Option".into(), vec![Ty::str()])));

            // set :: ConnRedis, Str, Str -> [net] Unit
            fields.insert("set".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // set_ex :: ConnRedis, Str, Str, Int -> [net] Unit
            fields.insert("set_ex".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str(), Ty::int()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // del :: ConnRedis, Str -> [net] Unit
            fields.insert("del".into(), Ty::function(
                vec![conn_t(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // exists :: ConnRedis, Str -> [net] Bool
            fields.insert("exists".into(), Ty::function(
                vec![conn_t(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::bool()));

            // expire :: ConnRedis, Str, Int -> [net] Unit
            fields.insert("expire".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::int()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // ---- Pub/Sub -------------------------------------------------

            // publish :: ConnRedis, Str, Str -> [net] Int
            // Returns the number of subscribers that received the message.
            fields.insert("publish".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::int()));

            // subscribe :: ConnRedis, Str, (Str, Str ->[E] Unit) -> [net] Nil
            // Blocking loop; handler receives (channel, message) on each message.
            // Uses a dedicated connection — Redis disallows non-Pub/Sub commands
            // on a subscribed connection. Handler carries an open effect row so
            // callers can use io, net, sql, etc. inside the closure.
            let handler2 = Ty::function(
                vec![Ty::str(), Ty::str()],
                EffectSet::open_var(0),
                Ty::Unit);
            fields.insert("subscribe".into(), Ty::function(
                vec![conn_t(), Ty::str(), handler2],
                EffectSet::singleton("net"),
                Ty::Unit));  // Nil = Unit

            // psubscribe :: ConnRedis, Str, (Str, Str, Str ->[E] Unit) -> [net] Nil
            // Pattern-subscribe; handler receives (pattern, channel, message).
            // Handler carries an open effect row (same rationale as subscribe).
            let handler3 = Ty::function(
                vec![Ty::str(), Ty::str(), Ty::str()],
                EffectSet::open_var(1),
                Ty::Unit);
            fields.insert("psubscribe".into(), Ty::function(
                vec![conn_t(), Ty::str(), handler3],
                EffectSet::singleton("net"),
                Ty::Unit));  // Nil = Unit

            // ---- List ----------------------------------------------------

            // lpush :: ConnRedis, Str, Str -> [net] Int
            fields.insert("lpush".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::int()));

            // rpush :: ConnRedis, Str, Str -> [net] Int
            fields.insert("rpush".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::int()));

            // brpop :: ConnRedis, Str, Int -> [net] Option[Str]
            // Blocking right-pop; returns None on timeout. timeout=0 blocks
            // indefinitely (the runtime does not treat this as a hung effect).
            fields.insert("brpop".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::int()],
                EffectSet::singleton("net"),
                Ty::Con("Option".into(), vec![Ty::str()])));

            // llen :: ConnRedis, Str -> [net] Int
            fields.insert("llen".into(), Ty::function(
                vec![conn_t(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::int()));

            // ---- Hash ----------------------------------------------------

            // hset :: ConnRedis, Str, Str, Str -> [net] Unit
            fields.insert("hset".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // hget :: ConnRedis, Str, Str -> [net] Option[Str]
            fields.insert("hget".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Con("Option".into(), vec![Ty::str()])));

            // hdel :: ConnRedis, Str, Str -> [net] Unit
            fields.insert("hdel".into(), Ty::function(
                vec![conn_t(), Ty::str(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::Unit));

            // hgetall :: ConnRedis, Str -> [net] List[(Str, Str)]
            fields.insert("hgetall".into(), Ty::function(
                vec![conn_t(), Ty::str()],
                EffectSet::singleton("net"),
                Ty::List(Box::new(Ty::Tuple(vec![Ty::str(), Ty::str()])))));

            Some(Ty::Record(fields))
        }
        "parser" => {
            // #217: structured parser combinators. Parser values are
            // tagged Records at runtime (`{ kind, ... }`), opaque at
            // the language level via `Ty::Con("Parser", [T])`.
            //
            // Surface:
            //   - primitives: char, string, digit, alpha, whitespace, eof
            //   - combinators: seq, alt, many, optional, map, and_then
            //   - run :: Parser[T], Str -> Result[T, ParseErr]
            //
            // `map` and `and_then` were deferred from #217's v1 because
            // their closure arguments carried call-site identity that
            // broke the canonical-parsers acceptance criterion. With
            // closure body-hash equality landed in #222, that concern
            // is gone, and #221 wires them in. The interpreter for
            // `parser.run` has been moved to `lex-bytecode::parser_runtime`
            // so it can invoke closures from `Map` / `AndThen` nodes.
            let pt = |t: Ty| Ty::Con("Parser".into(), vec![t]);
            let parse_err = || {
                let mut fs = IndexMap::new();
                fs.insert("pos".into(), Ty::int());
                fs.insert("message".into(), Ty::str());
                Ty::Record(fs)
            };
            let mut fields = IndexMap::new();
            // char :: Str -> Parser[Str] (single-char Str literal)
            fields.insert("char".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(), pt(Ty::str())));
            // string :: Str -> Parser[Str]
            fields.insert("string".into(), Ty::function(
                vec![Ty::str()], EffectSet::empty(), pt(Ty::str())));
            // digit :: () -> Parser[Str]
            fields.insert("digit".into(), Ty::function(
                vec![], EffectSet::empty(), pt(Ty::str())));
            // alpha :: () -> Parser[Str]
            fields.insert("alpha".into(), Ty::function(
                vec![], EffectSet::empty(), pt(Ty::str())));
            // whitespace :: () -> Parser[Str]
            fields.insert("whitespace".into(), Ty::function(
                vec![], EffectSet::empty(), pt(Ty::str())));
            // eof :: () -> Parser[Unit]
            fields.insert("eof".into(), Ty::function(
                vec![], EffectSet::empty(), pt(Ty::Unit)));
            // seq :: Parser[A], Parser[B] -> Parser[(A, B)]
            fields.insert("seq".into(), Ty::function(
                vec![pt(Ty::Var(0)), pt(Ty::Var(1))],
                EffectSet::empty(),
                pt(Ty::Tuple(vec![Ty::Var(0), Ty::Var(1)]))));
            // alt :: Parser[T], Parser[T] -> Parser[T]
            // PEG-style ordered choice: the second alternative is
            // tried only if the first fails.
            fields.insert("alt".into(), Ty::function(
                vec![pt(Ty::Var(0)), pt(Ty::Var(0))],
                EffectSet::empty(),
                pt(Ty::Var(0))));
            // many :: Parser[T] -> Parser[List[T]]
            // Zero-or-more. Stops as soon as the inner parser fails
            // OR doesn't advance the position (avoids infinite loop
            // on empty matches).
            fields.insert("many".into(), Ty::function(
                vec![pt(Ty::Var(0))],
                EffectSet::empty(),
                pt(Ty::List(Box::new(Ty::Var(0))))));
            // optional :: Parser[T] -> Parser[Option[T]]
            fields.insert("optional".into(), Ty::function(
                vec![pt(Ty::Var(0))],
                EffectSet::empty(),
                pt(Ty::Con("Option".into(), vec![Ty::Var(0)]))));
            // map :: Parser[T], (T) -> [E] U -> [E] Parser[U]
            // The closure runs at parse time when the Parser is run.
            // Effect-polymorphic on the closure: any effect the
            // closure declares propagates to the surrounding `run`.
            fields.insert("map".into(), Ty::function(
                vec![
                    pt(Ty::Var(0)),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(2), Ty::Var(1)),
                ],
                EffectSet::open_var(2),
                pt(Ty::Var(1))));
            // and_then :: Parser[T], (T) -> [E] Parser[U] -> [E] Parser[U]
            // Monadic bind: closure inspects the parsed value and
            // returns the next parser to run.
            fields.insert("and_then".into(), Ty::function(
                vec![
                    pt(Ty::Var(0)),
                    Ty::function(vec![Ty::Var(0)], EffectSet::open_var(3),
                        pt(Ty::Var(1))),
                ],
                EffectSet::open_var(3),
                pt(Ty::Var(1))));
            // run :: Parser[T], Str -> Result[T, ParseErr]
            // ParseErr = { pos :: Int, message :: Str }
            fields.insert("run".into(), Ty::function(
                vec![pt(Ty::Var(0)), Ty::str()],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), parse_err()])));
            Some(Ty::Record(fields))
        }
        "cli" => {
            // #224 Rubric port: argparse-equivalent for end-user
            // programs. Spec values are tagged `Json` records (opaque
            // to the language but inspectable). Construction via the
            // `flag` / `option` / `positional` / `spec` builders;
            // parse + introspection / help via the remaining ops.
            let json = || Ty::Con("Json".into(), vec![]);
            let opt_str = || Ty::Con("Option".into(), vec![Ty::str()]);
            let mut fields = IndexMap::new();
            // flag :: Str -> Option[Str] -> Str -> Json
            //   long_name -> short -> help -> CliArg
            fields.insert("flag".into(), Ty::function(
                vec![Ty::str(), opt_str(), Ty::str()],
                EffectSet::empty(),
                json()));
            // option :: Str -> Option[Str] -> Str -> Option[Str] -> Json
            //   long_name -> short -> help -> default -> CliArg
            fields.insert("option".into(), Ty::function(
                vec![Ty::str(), opt_str(), Ty::str(), opt_str()],
                EffectSet::empty(),
                json()));
            // positional :: Str -> Str -> Bool -> Json
            //   name -> help -> required -> CliArg
            fields.insert("positional".into(), Ty::function(
                vec![Ty::str(), Ty::str(), Ty::bool()],
                EffectSet::empty(),
                json()));
            // spec :: Str -> Str -> List[Json] -> List[Json] -> Json
            //   name -> help -> args -> subcommands -> CliSpec
            fields.insert("spec".into(), Ty::function(
                vec![Ty::str(), Ty::str(),
                     Ty::List(Box::new(json())),
                     Ty::List(Box::new(json()))],
                EffectSet::empty(),
                json()));
            // parse :: Json -> List[Str] -> Result[Json, Str]
            //   spec -> argv -> Result[CliParsed, error]
            fields.insert("parse".into(), Ty::function(
                vec![json(), Ty::List(Box::new(Ty::str()))],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![json(), Ty::str()])));
            // envelope :: Bool -> Str -> T -> Json
            //   ok -> command -> data -> ACLI-shaped envelope.
            // `data` is polymorphic so callers don't have to round-
            // trip through `json.parse` for trivial payloads.
            fields.insert("envelope".into(), Ty::function(
                vec![Ty::bool(), Ty::str(), Ty::Var(0)],
                EffectSet::empty(),
                json()));
            // describe :: Json -> Json — machine-readable spec dump
            fields.insert("describe".into(), Ty::function(
                vec![json()],
                EffectSet::empty(),
                json()));
            // help :: Json -> Str — human-readable help text
            fields.insert("help".into(), Ty::function(
                vec![json()],
                EffectSet::empty(),
                Ty::str()));
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
            // is_match_str :: Str, Str -> Bool
            // Compiles the first argument as a pattern and matches against the second.
            // Returns false on invalid pattern instead of propagating an error.
            fields.insert("is_match_str".into(), Ty::function(
                vec![Ty::str(), Ty::str()], EffectSet::empty(), Ty::bool()));
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
            // stream_lines :: Str, Map[Str, Str], Str -> [net] Result[Iter[Str], Str]
            // Streaming HTTP POST; yields the response body line-by-line for
            // SSE / NDJSON endpoints. Connection errors surface as Err(Str).
            fields.insert("stream_lines".into(), Ty::function(
                vec![
                    Ty::str(),
                    Ty::Con("Map".into(), vec![Ty::str(), Ty::str()]),
                    Ty::str(),
                ],
                EffectSet::singleton("net"),
                Ty::Con("Result".into(), vec![
                    Ty::Con("Iter".into(), vec![Ty::str()]),
                    Ty::str(),
                ]),
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
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::empty(),
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
            // field access panic later.
            fields.insert("parse_strict".into(), Ty::function(
                vec![Ty::str(), Ty::List(Box::new(Ty::str()))],
                EffectSet::empty(),
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
            // cloud_stream :: Str -> [llm_cloud] Result[Stream[Str], Str]
            // (#305 slice 3). Streaming counterpart to cloud_complete.
            // The result is `Result[Stream[Str], Str]` rather than a
            // bare Stream so transport errors surface synchronously
            // at handshake time; per-chunk errors collapse the
            // stream to early termination.
            fields.insert("cloud_stream".into(), Ty::function(
                vec![Ty::str()],
                EffectSet::singleton("llm_cloud"),
                Ty::Con("Result".into(), vec![
                    Ty::Con("Stream".into(), vec![Ty::str()]),
                    Ty::str(),
                ]),
            ));
            Some(Ty::Record(fields))
        }
        "stream" => {
            // #305 slice 3: opaque consumer-side operations on
            // `Stream[T]`. Producers live elsewhere (`agent.cloud_stream`
            // for now); future producers (`http.get_stream`, etc.)
            // will register the same Stream[T] surface.
            let mut fields = IndexMap::new();
            // next :: Stream[T] -> [stream] Option[T]
            // One pull. `None` signals end-of-stream (consumed by
            // the producer's lazy generator).
            fields.insert("next".into(), Ty::function(
                vec![Ty::Con("Stream".into(), vec![Ty::Var(0)])],
                EffectSet::singleton("stream"),
                Ty::Con("Option".into(), vec![Ty::Var(0)]),
            ));
            // collect :: Stream[T] -> [stream] List[T]
            // Drain to a list. Eager; blocks until the producer
            // signals end-of-stream.
            fields.insert("collect".into(), Ty::function(
                vec![Ty::Con("Stream".into(), vec![Ty::Var(0)])],
                EffectSet::singleton("stream"),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            Some(Ty::Record(fields))
        }
        // -- std.decimal (#574): exact decimal arithmetic with explicit rounding.
        // `Decimal = { coefficient :: Int, exponent :: Int }` where the value
        // is `coefficient × 10^exponent`.  All arithmetic is exact (no IEEE 754
        // approximation); rounding only happens at `round_to`, which demands an
        // explicit mode string ("HalfUp" | "HalfDown" | "HalfEven" |
        // "Down" | "Up" | "Ceiling" | "Floor").
        "decimal" => {
            // Local helper: the Decimal record type.
            let decimal_ty = || {
                let mut f = IndexMap::new();
                f.insert("coefficient".into(), Ty::int());
                f.insert("exponent".into(), Ty::int());
                Ty::Record(f)
            };
            let mut fields = IndexMap::new();
            // Constructors
            // decimal :: (Int, Int) -> Decimal — coefficient, exponent
            fields.insert("decimal".into(), Ty::function(
                vec![Ty::int(), Ty::int()], EffectSet::empty(), decimal_ty()));
            // zero :: () -> Decimal — 0 × 10^0
            fields.insert("zero".into(), Ty::function(
                vec![], EffectSet::empty(), decimal_ty()));
            // one :: () -> Decimal — 1 × 10^0
            fields.insert("one".into(), Ty::function(
                vec![], EffectSet::empty(), decimal_ty()));
            // from_int :: Int -> Decimal — lift integer, exponent=0
            fields.insert("from_int".into(), Ty::function(
                vec![Ty::int()], EffectSet::empty(), decimal_ty()));
            // Arithmetic — all exact, no rounding
            // add :: (Decimal, Decimal) -> Decimal
            fields.insert("add".into(), Ty::function(
                vec![decimal_ty(), decimal_ty()], EffectSet::empty(), decimal_ty()));
            // sub :: (Decimal, Decimal) -> Decimal
            fields.insert("sub".into(), Ty::function(
                vec![decimal_ty(), decimal_ty()], EffectSet::empty(), decimal_ty()));
            // mul :: (Decimal, Decimal) -> Decimal — exponents add
            fields.insert("mul".into(), Ty::function(
                vec![decimal_ty(), decimal_ty()], EffectSet::empty(), decimal_ty()));
            // Comparison — three-way: -1 / 0 / 1
            // compare :: (Decimal, Decimal) -> Int
            fields.insert("compare".into(), Ty::function(
                vec![decimal_ty(), decimal_ty()], EffectSet::empty(), Ty::int()));
            // Predicates
            fields.insert("is_zero".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), Ty::bool()));
            fields.insert("is_positive".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), Ty::bool()));
            fields.insert("is_negative".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), Ty::bool()));
            // Transformers
            // normalize :: Decimal -> Decimal — remove trailing zeros
            fields.insert("normalize".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), decimal_ty()));
            // negate :: Decimal -> Decimal
            fields.insert("negate".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), decimal_ty()));
            // abs :: Decimal -> Decimal
            fields.insert("abs".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), decimal_ty()));
            // round_to :: (Decimal, Int, Str) -> Decimal
            //   target_exp: the exponent to round to (e.g. -2 → 2 decimal places)
            //   mode: "HalfUp" | "HalfDown" | "HalfEven" | "Down" | "Up" | "Ceiling" | "Floor"
            fields.insert("round_to".into(), Ty::function(
                vec![decimal_ty(), Ty::int(), Ty::str()],
                EffectSet::empty(), decimal_ty()));
            // to_str :: Decimal -> Str — decimal notation, e.g. "123.45"
            fields.insert("to_str".into(), Ty::function(
                vec![decimal_ty()], EffectSet::empty(), Ty::str()));
            // pow10 :: Int -> Int — 10^n; n must be in [0, 18]
            fields.insert("pow10".into(), Ty::function(
                vec![Ty::int()], EffectSet::empty(), Ty::int()));
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
        "random" => "random",
        "env" => "env",
        "bytes" => "bytes",
        "net" => "net",
        "tls" => "tls",
        "chat" => "chat",
        "math" => "math",
        "map" => "map",
        "set" => "set",
        "iter" => "iter",
        "crypto" => "crypto",
        "regex" => "regex",
        "parser" => "parser",
        "deque" => "deque",
        "kv" => "kv",
        "sql" => "sql",
        "fs" => "fs",
        "process" => "process",
        "datetime" => "datetime",
        "duration" => "duration",
        "log" => "log",
        "http" => "http",
        "toml" => "toml",
        "yaml" => "yaml",
        "dotenv" => "dotenv",
        "csv" => "csv",
        "test" => "test",
        "agent" => "agent",
        "cli" => "cli",
        "stream" => "stream",
        "conc" => "conc",
        "arrow" => "arrow",
        "df" => "df",
        "redis" => "redis",
        "decimal" => "decimal",
        _ => return None,
    })
}
