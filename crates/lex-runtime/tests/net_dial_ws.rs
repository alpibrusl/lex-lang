//! Wire-level integration tests for `net.dial_ws` — the WebSocket
//! client primitive that mirrors `net.serve_ws_fn` (#390).
//!
//! Topology: a small Rust `tungstenite` server runs in a background
//! thread, the test main thread drives a Lex program that uses
//! `net.dial_ws` to connect, exchange a few frames, then exit when
//! the server closes the connection.
//!
//! We use a Rust server rather than Lex `net.serve_ws_fn` for two
//! reasons:
//!  1. `serve_ws_fn` blocks forever — we'd need an out-of-band shutdown
//!     hook to drive a single-test exchange, which the current API
//!     doesn't expose.
//!  2. Verifying the dial-side behaviour through what the server
//!     *received* gives us an end-to-end correctness signal without
//!     having to thread state out of the Lex VM.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tungstenite::Message;

/// Spin up a one-shot WS server bound to an OS-assigned port.
///
/// `setup` runs against the accepted WebSocket immediately after the
/// handshake (server-initiated frames go here). The server then
/// loops, recording every text frame it receives into `recv_log`,
/// optionally responding via `on_each`, until `max_frames` have
/// been consumed; then it closes.
fn spawn_test_server(
    setup: impl FnOnce(&mut tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
    on_each: impl Fn(&str, &mut tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
    max_frames: usize,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let recv_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_handle = Arc::clone(&recv_log);

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("server accept");
        // Read timeout so the server doesn't hang forever if the
        // client misbehaves; the test as a whole still gets bounded
        // by cargo test's own deadline.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut ws = tungstenite::accept(stream).expect("ws handshake");
        setup(&mut ws);

        while log_handle.lock().unwrap().len() < max_frames {
            match ws.read() {
                Ok(Message::Text(body)) => {
                    let s = body.to_string();
                    log_handle.lock().unwrap().push(s.clone());
                    on_each(&s, &mut ws);
                }
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = ws.close(None);
        // Drain a tail close frame.
        let _ = ws.read();
    });

    // Give the listener a tick to be ready before the client dials.
    thread::sleep(Duration::from_millis(50));
    (port, recv_log)
}

fn run_lex(src: &str, fn_name: &str) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, vec![])
        .unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn assert_ok_unit(v: &Value) {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => {
            assert!(matches!(args[0], Value::Unit), "Ok payload not Unit: {:?}", args[0]);
        }
        other => panic!("expected Ok(Unit), got {other:?}"),
    }
}

fn assert_err_contains(v: &Value, needle: &str) {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => match &args[0] {
            Value::Str(s) => assert!(
                s.contains(needle),
                "expected Err containing `{needle}`, got `{s}`"
            ),
            other => panic!("Err payload not Str: {other:?}"),
        },
        other => panic!("expected Err(_), got {other:?}"),
    }
}

#[test]
fn dial_ws_runs_on_open_then_replies_to_inbound_text() {
    // Server: after handshake, send "ping" once. Wait for the client's
    // boot frame + its pong reply, then close.
    let (port, log) = spawn_test_server(
        |ws| {
            ws.send(Message::Text("ping".into())).expect("server ping");
        },
        |_inbound, _ws| {},
        2, // boot + pong
    );

    let src = format!(
        r#"
import "std.net" as net

fn main() -> [net] Result[Unit, Str] {{
  net.dial_ws(
    "ws://127.0.0.1:{port}",
    "",
    fn () -> WsAction {{ WsSend("boot") }},
    fn (msg :: WsMessage) -> WsAction {{
      match msg {{
        WsText(_)  => WsSend("pong"),
        WsBinary(_) => WsNoOp,
        WsPing     => WsNoOp,
        WsClose    => WsNoOp,
      }}
    }},
  )
}}
"#
    );

    let result = run_lex(&src, "main");
    assert_ok_unit(&result);

    let frames = log.lock().unwrap().clone();
    assert_eq!(
        frames,
        vec!["boot".to_string(), "pong".to_string()],
        "server should have seen boot frame from on_open and pong reply from on_message",
    );
}

#[test]
fn dial_ws_returns_err_on_connect_failure() {
    // No server bound — picking a port that's almost certainly closed.
    let src = r#"
import "std.net" as net

fn main() -> [net] Result[Unit, Str] {
  net.dial_ws(
    "ws://127.0.0.1:1",
    "",
    fn () -> WsAction { WsNoOp },
    fn (_msg :: WsMessage) -> WsAction { WsNoOp },
  )
}
"#;
    let result = run_lex(src, "main");
    assert_err_contains(&result, "connect");
}

#[test]
fn dial_ws_returns_err_on_bad_url() {
    let src = r#"
import "std.net" as net

fn main() -> [net] Result[Unit, Str] {
  net.dial_ws(
    "not a url",
    "",
    fn () -> WsAction { WsNoOp },
    fn (_msg :: WsMessage) -> WsAction { WsNoOp },
  )
}
"#;
    let result = run_lex(src, "main");
    // A `relative URL without a base` or `invalid scheme` style message —
    // the exact wording is tungstenite's; we just need it to surface
    // as a Lex `Err`.
    match &result {
        Value::Variant { name, .. } if name == "Err" => {}
        other => panic!("expected Err(_), got {other:?}"),
    }
}

#[test]
fn dial_ws_subprotocol_header_is_sent_when_non_empty() {
    // Bind a server that records the Sec-WebSocket-Protocol header
    // out of the handshake request and then closes immediately.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let seen_subproto: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let seen_handle = Arc::clone(&seen_subproto);

    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut ws = tungstenite::accept_hdr(stream, |req: &tungstenite::handshake::server::Request, mut resp: tungstenite::handshake::server::Response| {
            if let Some(v) = req.headers().get("Sec-WebSocket-Protocol") {
                if let Ok(s) = v.to_str() {
                    *seen_handle.lock().unwrap() = Some(s.to_string());
                    // Echo back so tungstenite considers the negotiation valid.
                    resp.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        tungstenite::http::HeaderValue::from_str(s).unwrap(),
                    );
                }
            }
            Ok(resp)
        }).expect("handshake");
        let _ = ws.close(None);
        let _ = ws.read();
    });

    thread::sleep(Duration::from_millis(50));

    let src = format!(
        r#"
import "std.net" as net

fn main() -> [net] Result[Unit, Str] {{
  net.dial_ws(
    "ws://127.0.0.1:{port}",
    "ocpp1.6",
    fn () -> WsAction {{ WsNoOp }},
    fn (_msg :: WsMessage) -> WsAction {{ WsNoOp }},
  )
}}
"#
    );

    let result = run_lex(&src, "main");
    assert_ok_unit(&result);

    let seen = seen_subproto.lock().unwrap().clone();
    assert_eq!(
        seen.as_deref(),
        Some("ocpp1.6"),
        "server should have received Sec-WebSocket-Protocol: ocpp1.6"
    );
}
