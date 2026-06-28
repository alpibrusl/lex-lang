//! Regression for lex-lang#698: `conc.ask` on an actor, called from inside a
//! `net.serve_fn` request handler, used to fail with
//!   "conc.ask: handler must return a 2-tuple (new_state, reply), got ArenaTuple …"
//! because the handler's `(new_state, reply)` tuple is arena-allocated under the
//! serve worker's request scope and the validation only accepted a heap
//! `Value::Tuple`. The fix materializes arena handles before the check. Here we
//! spawn the actor in `main` and `ask` it from the GET handler; the reply must
//! come back as the actor's state value, not an internal error.

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
    // #698 needs concurrent (actor) + net (serve) + io.
    policy.allow_effects = ["net", "concurrent", "io"]
        .into_iter()
        .map(String::from)
        .collect::<BTreeSet<_>>();
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

const PROG: &str = r#"
import "std.conc" as conc
import "std.net" as net
import "std.io" as io
import "std.int" as int
import "std.map" as map

type St = { n :: Int }
type M = Bump | Ask
type R = Done | Val(Int)

fn handler(s :: St, m :: M) -> (St, R) {
  match m {
    Bump => ({ n: s.n + 1 }, Done),
    Ask => (s, Val(s.n)),
  }
}

fn main() -> [concurrent, io, net] Unit {
  let a := conc.spawn({ n: 7 }, handler)
  let h := fn (req :: Request) -> [concurrent, io, net] Response {
    let r := match conc.ask(a, Ask) {
      Val(x) => int.to_str(x),
      Done => "done",
    }
    { status: 200, body: BodyStr(r), headers: map.from_list([]) }
  }
  net.serve_fn(9711, h)
}
"#;

#[test]
fn conc_ask_from_serve_handler_returns_reply() {
    spawn_lex_server(PROG, "main");
    wait_for_bind(9711, Duration::from_secs(10));
    let (status, body) = http_get(9711, "/");
    assert_eq!(status, 200, "unexpected status; body = {body:?}");
    assert_eq!(
        body.trim(),
        "7",
        "conc.ask from a serve handler should return the actor's state value, \
         not an internal error (lex-lang#698); got body = {body:?}"
    );
}
