//! Regression test for alpibrusl/lex-lang#421.
//!
//! `net.serve_ws_fn(port, subprotocol, handler)` must echo the
//! negotiated `Sec-WebSocket-Protocol` header back in the handshake
//! response when the client requested a subprotocol — otherwise
//! strict WS clients (including `net.dial_ws`, and tungstenite ≥
//! 0.20 generally) reject the handshake with
//! "SubProtocol error: Server sent no subprotocol", per RFC 6455
//! §4.1.
//!
//! The check_program / VM machinery is identical to ws_chat.rs;
//! we just need a serve_ws_fn server in a background thread and
//! a strict client driving the handshake from the test main thread.
//!
//! The server thread leaks (serve_ws_fn blocks forever — same
//! limitation as ws_chat.rs); cargo test reaps it on process exit.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;

fn spawn_serve_ws_fn(src: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(300));
}

fn ws_src_with_subprotocol(port: u16, subprotocol: &str) -> String {
    format!(
        r#"
import "std.net" as net

fn on_message(_c :: WsConn, _m :: WsMessage) -> WsAction {{ WsNoOp }}

fn main() -> [net] Nil {{
  net.serve_ws_fn({port}, "{subprotocol}", on_message)
}}
"#
    )
}

#[test]
fn serve_ws_fn_echoes_subprotocol_to_strict_client() {
    let port = 19877;
    spawn_serve_ws_fn(&ws_src_with_subprotocol(port, "ocpp1.6"));

    let url = format!("ws://127.0.0.1:{port}/test");
    let mut req = url
        .as_str()
        .into_client_request()
        .expect("build client request");
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        tungstenite::http::HeaderValue::from_static("ocpp1.6"),
    );

    let (mut ws, resp) =
        tungstenite::connect(req).expect("ws handshake (regression #421)");

    let echoed = resp
        .headers()
        .get("Sec-WebSocket-Protocol")
        .expect("server must echo Sec-WebSocket-Protocol");
    assert_eq!(
        echoed.to_str().unwrap(),
        "ocpp1.6",
        "echoed subprotocol must match the server's configuration"
    );

    let _ = ws.close(None);
}

#[test]
fn serve_ws_fn_omits_subprotocol_header_when_server_subprotocol_is_empty() {
    // Symmetric case: when the server is configured with "" and the
    // client asks for nothing either, no Sec-WebSocket-Protocol
    // header should appear in the response. (Tests the negative
    // branch of the echo condition.)
    let port = 19878;
    spawn_serve_ws_fn(&ws_src_with_subprotocol(port, ""));

    let url = format!("ws://127.0.0.1:{port}/test");
    let (mut ws, resp) =
        tungstenite::connect(url.as_str()).expect("ws handshake");

    assert!(
        resp.headers().get("Sec-WebSocket-Protocol").is_none(),
        "no echo when subprotocol negotiation isn't in play"
    );

    let _ = ws.close(None);
}
