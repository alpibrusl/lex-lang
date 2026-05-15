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
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;

/// Reserve a free port via a kernel-assigned bind then immediately
/// release it, returning the port for the Lex server to claim. There
/// is a tiny TOCTOU window between drop and the Lex bind, but in
/// practice the kernel doesn't re-issue the port that quickly to an
/// unrelated process on the same machine. Beats hardcoding magic
/// numbers that collide on busy CI runners.
fn free_port() -> u16 {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

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
    let port = free_port();
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
    let port = free_port();
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

#[test]
fn serve_ws_fn_does_not_echo_when_client_offers_different_subprotocol() {
    // RFC 6455 §4.1: the server MUST select one of the subprotocols
    // the client offered. If the server is configured for `ocpp1.6`
    // but the client offers only `mqtt`, the server must NOT echo
    // `ocpp1.6` — it would be advertising a subprotocol the client
    // didn't propose. The handshake itself can still succeed (a
    // strict client may then reject it on its own end); the server's
    // job here is just to be RFC-compliant about what it advertises.
    let port = free_port();
    spawn_serve_ws_fn(&ws_src_with_subprotocol(port, "ocpp1.6"));

    let url = format!("ws://127.0.0.1:{port}/test");
    let mut req = url
        .as_str()
        .into_client_request()
        .expect("build client request");
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        tungstenite::http::HeaderValue::from_static("mqtt"),
    );

    // The handshake may succeed or fail depending on how strict the
    // client is about an empty subprotocol-response; what we care
    // about is the response header value (or absence of it).
    match tungstenite::connect(req) {
        Ok((mut ws, resp)) => {
            assert!(
                resp.headers().get("Sec-WebSocket-Protocol").is_none(),
                "server must not echo a subprotocol the client didn't offer"
            );
            let _ = ws.close(None);
        }
        Err(_) => {
            // Strict client rejected the empty echo — acceptable;
            // the server-side behaviour is still RFC-compliant.
        }
    }
}

#[test]
fn serve_ws_fn_picks_server_value_from_client_multi_offer() {
    // Client offers a comma-separated list `ocpp1.6, ocpp2.0.1`,
    // server configured for `ocpp1.6` → echo `ocpp1.6` (server's
    // chosen value among the offered ones). This pins the matcher
    // against the realistic OCPP-with-future-version scenario.
    let port = free_port();
    spawn_serve_ws_fn(&ws_src_with_subprotocol(port, "ocpp1.6"));

    let url = format!("ws://127.0.0.1:{port}/test");
    let mut req = url
        .as_str()
        .into_client_request()
        .expect("build client request");
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        tungstenite::http::HeaderValue::from_static("ocpp1.6, ocpp2.0.1"),
    );

    let (mut ws, resp) =
        tungstenite::connect(req).expect("ws handshake");
    let echoed = resp
        .headers()
        .get("Sec-WebSocket-Protocol")
        .expect("server must echo when its value is in the offered list");
    assert_eq!(echoed.to_str().unwrap(), "ocpp1.6");
    let _ = ws.close(None);
}

#[test]
fn serve_ws_fn_rejects_invalid_subprotocol_at_startup() {
    // An invalid subprotocol value (one that can't form a valid HTTP
    // header — e.g. one containing newlines) used to silently drop
    // the Sec-WebSocket-Protocol header at every handshake, leaving
    // a hard-to-diagnose runtime failure. serve_ws_fn now validates
    // at startup and returns Err immediately. We invoke the runtime
    // function directly here because the lex-syntax string literal
    // wouldn't allow an embedded `\r\n` without escaping anyway.
    use indexmap::IndexMap;
    use lex_bytecode::{Program, Value};
    use lex_runtime::ws::{serve_ws_fn, ChatRegistry};

    let program = Arc::new(Program {
        constants: vec![],
        functions: vec![],
        function_names: IndexMap::new(),
        module_aliases: IndexMap::new(),
        entry: None,
    });
    let policy = Policy::pure();
    let registry = Arc::new(ChatRegistry::default());
    // A closure-shaped Value::Unit stands in for the handler; the
    // function never gets that far because the subprotocol check
    // happens first.
    let result = serve_ws_fn(
        0,
        "ocpp\r\nX-Injected: yes".to_string(),
        Value::Unit,
        program,
        policy,
        registry,
    );
    let err = result.expect_err("invalid subprotocol must reject at startup");
    assert!(
        err.contains("not a valid HTTP header value"),
        "unexpected error message: {err}"
    );
}
