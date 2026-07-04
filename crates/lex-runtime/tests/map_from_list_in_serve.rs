//! Regression for the `map.from_list` / `ArenaTuple` bug: any tuple built
//! inside a `net.serve_fn` request handler (e.g. by a closure passed to
//! `list.map`) is arena-allocated under the live request scope, but every
//! pure builtin downstream — `map.from_list` here — pattern-matches on the
//! heap `Value::Tuple` shape and has no arena access to resolve a handle
//! itself. Before the fix this failed with:
//!   "internal error: effect handler error: map.from_list element must be a
//!    2-tuple, got ArenaTuple { slab_start: .., arity: 2 }"
//! for ANY GET request whose handler builds tuples and feeds them to a
//! builtin that inspects their shape — which is exactly the `ctx.query_map` /
//! `ctx.cookie_map` pattern in lex-web (`str.split` + closure returning a
//! tuple + `map.from_list`), i.e. every `?query=param` and `Cookie:` header on
//! a live server. The fix materializes every `Op::EffectCall` arg's arena
//! handles before dispatch, at the single choke point every effect/builtin
//! call passes through — not just the one call site that happened to be hit
//! first (`conc.ask`, lex-lang#698).

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn spawn_lex_server(src: &str, entry: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net", "io"].into_iter().map(String::from).collect::<BTreeSet<_>>();
    let entry = entry.to_string();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call(&entry, vec![]);
    });
}

fn wait_for_bind(port: u16, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let mut backoff = Duration::from_millis(20);
    loop {
        if let Ok(s) = TcpStream::connect_timeout(
            &("127.0.0.1", port).to_socket_addrs().unwrap().next().unwrap(),
            Duration::from_millis(200),
        ) {
            drop(s);
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("server on :{port} did not bind within {timeout:?}");
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}

fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

// Mirrors the exact pattern in lex-web's `ctx.query_map`/`cookie_map`: split a
// string into parts, build a `(Str, Str)` tuple per part inside a closure
// passed to `list.map`, then hand the whole list to `map.from_list`.
const PROG: &str = r#"
import "std.net" as net
import "std.io" as io
import "std.map" as map
import "std.list" as list
import "std.str" as str

fn head_or(parts :: List[Str], d :: Str) -> Str {
  match list.head(parts) {
    Some(s) => s,
    None => d,
  }
}

fn main() -> [io, net] Unit {
  let h := fn (req :: Request) -> [io, net] Response {
    let pairs := list.map(["a=1", "b=2"], fn (kv :: Str) -> (Str, Str) {
      let parts := str.split(kv, "=")
      (head_or(parts, ""), "v")
    })
    let m := map.from_list(pairs)
    { status: 200, body: BodyStr("ok"), headers: map.from_list([]) }
  }
  net.serve_fn(9723, h)
}
"#;

#[test]
fn map_from_list_of_closure_built_tuples_works_in_serve_handler() {
    spawn_lex_server(PROG, "main");
    wait_for_bind(9723, Duration::from_secs(10));
    let (status, body) = http_get(9723, "/");
    assert_eq!(
        status, 200,
        "map.from_list over closure-built tuples should succeed inside a serve \
         handler, not reject a valid arena-allocated tuple; body = {body:?}"
    );
    assert_eq!(body.trim(), "ok");
}
