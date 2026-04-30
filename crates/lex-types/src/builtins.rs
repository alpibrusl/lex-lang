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
            Some(Ty::Record(fields))
        }
        "int" => {
            let mut fields = IndexMap::new();
            fields.insert("to_str".into(), Ty::function(vec![Ty::int()], EffectSet::empty(), Ty::str()));
            fields.insert("to_float".into(), Ty::function(vec![Ty::int()], EffectSet::empty(), Ty::float()));
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
            // map :: List[a], (a) -> b -> List[b]
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                ],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(1))),
            ));
            fields.insert("filter".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::bool()),
                ],
                EffectSet::empty(),
                Ty::List(Box::new(Ty::Var(0))),
            ));
            fields.insert("fold".into(), Ty::function(
                vec![
                    Ty::List(Box::new(Ty::Var(0))),
                    Ty::Var(1),
                    Ty::function(vec![Ty::Var(1), Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                ],
                EffectSet::empty(),
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
            Some(Ty::Record(fields))
        }
        "result" => {
            let mut fields = IndexMap::new();
            // result.map :: Result[T, E], (T) -> U -> Result[U, E]
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(2)),
                ],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)]),
            ));
            fields.insert("and_then".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(),
                        Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)])),
                ],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(2), Ty::Var(1)]),
            ));
            fields.insert("map_err".into(), Ty::function(
                vec![
                    Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)]),
                    Ty::function(vec![Ty::Var(1)], EffectSet::empty(), Ty::Var(2)),
                ],
                EffectSet::empty(),
                Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)]),
            ));
            Some(Ty::Record(fields))
        }
        "option" => {
            let mut fields = IndexMap::new();
            fields.insert("map".into(), Ty::function(
                vec![
                    Ty::Con("Option".into(), vec![Ty::Var(0)]),
                    Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(1)),
                ],
                EffectSet::empty(),
                Ty::Con("Option".into(), vec![Ty::Var(1)]),
            ));
            fields.insert("unwrap_or".into(), Ty::function(
                vec![Ty::Con("Option".into(), vec![Ty::Var(0)]), Ty::Var(0)],
                EffectSet::empty(),
                Ty::Var(0),
            ));
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
        "bytes" => "bytes",
        "net" => "net",
        _ => return None,
    })
}
