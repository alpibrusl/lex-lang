# Small helpers over `std.json`'s generic `Json` ADT
# (JNull | JBool | JInt | JFloat | JStr | JList | JObj), used to walk
# the `lex --output json docs <path>` envelope without a schema-specific
# deserializer. See docs.rs's `ApiDocs` / `ModuleDoc` / `FnDoc` structs
# (crates/lex-cli/src/docs.rs) for the shape these helpers navigate:
#   { ok, command, data: { lex_docs_version, package?, version, modules },
#     meta }
#   module:   { file, doc?, functions }
#   function: { name, sig_id?, signature, effects, examples?, doc? }

import "std.list" as list
import "std.tuple" as tuple
import "std.str" as str

fn obj_field(kvs :: List[Tuple[Str, Json]], key :: Str) -> Option[Json] {
  list.fold(kvs, None, fn (acc :: Option[Json], kv :: Tuple[Str, Json]) -> Option[Json] {
    match acc {
      Some(v) => Some(v),
      None => match tuple.fst(kv) == key {
        true => Some(tuple.snd(kv)),
        false => None,
      },
    }
  })
}

fn as_str(j :: Json) -> Str {
  match j { JStr(s) => s, _ => "" }
}

fn as_list(j :: Json) -> List[Json] {
  match j { JList(xs) => xs, _ => [] }
}

fn as_obj(j :: Json) -> List[Tuple[Str, Json]] {
  match j { JObj(kvs) => kvs, _ => [] }
}

# `obj.field` as a Str, defaulting to "" when absent or not a JStr.
fn field_str(obj :: List[Tuple[Str, Json]], key :: Str) -> Str {
  match obj_field(obj, key) { Some(v) => as_str(v), None => "" }
}

# `obj.field` as a Str, `None` when the key is entirely absent (as
# opposed to present-but-empty) — used where "no doc comment" and "an
# empty doc comment" should render differently.
fn field_str_opt(obj :: List[Tuple[Str, Json]], key :: Str) -> Option[Str] {
  match obj_field(obj, key) { Some(v) => Some(as_str(v)), None => None }
}

fn field_list(obj :: List[Tuple[Str, Json]], key :: Str) -> List[Json] {
  match obj_field(obj, key) { Some(v) => as_list(v), None => [] }
}

fn str_list(js :: List[Json]) -> List[Str] {
  list.map(js, fn (j :: Json) -> Str { as_str(j) })
}
