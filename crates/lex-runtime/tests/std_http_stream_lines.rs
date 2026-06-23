//! #683: `http.stream_lines` returns a lazy `Stream[Str]` and streams the
//! response body line-by-line, rather than buffering the whole body into an
//! eager `Iter[Str]`. Drives a tiny in-process HTTP server so the test needs
//! no network.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// Spawn a one-shot HTTP/1.1 server that replies to a single POST with
/// `body`, then closes. Returns the bound `http://127.0.0.1:PORT/` URL.
fn spawn_server(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            // Read the request headers (and any body) until the client pauses;
            // we don't need to parse it, just drain enough not to deadlock.
            let mut buf = [0u8; 4096];
            let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    format!("http://127.0.0.1:{port}/")
}

const SRC: &str = r#"
import "std.http"   as http
import "std.stream" as stream
import "std.map"    as map

fn fetch(url :: Str) -> [net, stream] List[Str] {
  match http.stream_lines(url, map.new(), "") {
    Ok(s) => stream.collect(s),
    Err(e) => [e],
  }
}

# Pull just the first line via stream.next, proving incremental consumption.
fn first(url :: Str) -> [net, stream] Str {
  match http.stream_lines(url, map.new(), "") {
    Ok(s) => match stream.next(s) {
      Some(line) => line,
      None => "EMPTY",
    },
    Err(e) => e,
  }
}
"#;

fn run(fn_name: &str, url: &str) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::permissive()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, vec![Value::Str(url.into())]).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

#[test]
fn stream_collect_yields_each_line() {
    let url = spawn_server("line1\nline2\nline3\n");
    let v = run("fetch", &url);
    let lines = match v {
        Value::List(items) => items.into_iter().map(|x| match x {
            Value::Str(s) => s.to_string(),
            other => panic!("expected Str, got {other:?}"),
        }).collect::<Vec<_>>(),
        other => panic!("expected List, got {other:?}"),
    };
    assert_eq!(lines, vec!["line1", "line2", "line3"]);
}

#[test]
fn stream_next_pulls_first_line() {
    let url = spawn_server("first-event\nsecond-event\n");
    assert_eq!(run("first", &url), Value::Str("first-event".into()));
}
