//! #684: `http.json_body[T]` gets the same required-field / type / Option
//! validation as `json.parse_strict` when `T` is a record. The
//! type-checker rewrite turns `http.json_body(resp)` into
//! `http.json_body_typed(resp, [required], [schema])`; the runtime then
//! returns a `DecodeError` (HttpError) instead of an `Ok` with a
//! silently-incomplete record.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.http"  as http
import "std.bytes" as bytes
import "std.map"   as map

type User = { id :: Int, name :: Str, nick :: Option[Str] }

fn decode(body_json :: Str) -> Result[User, HttpError] {
  http.json_body({ status: 200, headers: map.new(), body: bytes.from_str(body_json) })
}

fn name_of(body_json :: Str) -> Str {
  match decode(body_json) { Ok(u) => u.name, Err(_) => "ERR" }
}
fn is_err(body_json :: Str) -> Bool {
  match decode(body_json) { Ok(_) => false, Err(_) => true }
}
fn nick_of(body_json :: Str) -> Option[Str] {
  match decode(body_json) { Ok(u) => u.nick, Err(_) => None }
}
"#;

fn run(fn_name: &str, json: &str) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let mut stages = canonicalize_program(&prog);
    lex_types::check_and_rewrite_program(&mut stages)
        .unwrap_or_else(|errs| panic!("type errors:\n{errs:#?}"));
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::permissive()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, vec![Value::Str(json.into())])
        .unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

#[test]
fn complete_object_decodes_ok() {
    assert_eq!(run("name_of", r#"{"id":1,"name":"Alice","nick":"al"}"#),
        Value::Str("Alice".into()));
    assert_eq!(run("is_err", r#"{"id":1,"name":"Alice","nick":"al"}"#),
        Value::Bool(false));
}

#[test]
fn missing_required_field_is_decode_error() {
    // `name` is required (non-Option) but absent → Err, not Ok(incomplete).
    assert_eq!(run("is_err", r#"{"id":1}"#), Value::Bool(true));
}

#[test]
fn wrong_typed_field_is_decode_error() {
    // `id` must be Int; a string must be rejected.
    assert_eq!(run("is_err", r#"{"id":"nope","name":"Alice"}"#), Value::Bool(true));
}

#[test]
fn option_field_wraps_some_and_none() {
    assert_eq!(run("nick_of", r#"{"id":1,"name":"Alice","nick":"al"}"#),
        Value::Variant { name: "Some".into(), args: vec![Value::Str("al".into())] });
    // Absent optional field → None, and the object still decodes Ok.
    assert_eq!(run("nick_of", r#"{"id":1,"name":"Alice"}"#),
        Value::Variant { name: "None".into(), args: vec![] });
    assert_eq!(run("is_err", r#"{"id":1,"name":"Alice"}"#), Value::Bool(false));
}
