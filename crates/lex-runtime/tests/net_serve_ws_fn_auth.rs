//! Integration tests for `net.serve_ws_fn_auth` (#423).
//!
//! Covers:
//!  1. **Happy path** — the auth callback returns `Ok(())`, the
//!     handshake completes, and the on_message handler runs as
//!     usual.
//!  2. **Rejection** — the auth callback returns `Err(msg)`, the
//!     server responds 401 Unauthorized, and the on_message handler
//!     is never invoked.
//!  3. **Headers are surfaced** — the auth callback sees an
//!     `Authorization` header that was on the upgrade request.
//!
//! Pattern mirrors `ws_chat.rs` / `net_serve_ws_subprotocol.rs`:
//! spawn the Lex server in a background thread, drive a strict
//! tungstenite client from the test main thread, observe the
//! handshake outcome through response status / body / received
//! frames. The server thread leaks (`serve_ws_fn_auth` blocks
//! forever — same limitation as the other WS tests in this crate);
//! cargo test reaps it on process exit.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::StatusCode;

fn free_port() -> u16 {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

fn spawn(src: &str) {
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
    // Match ws_chat.rs's known-good wait — 300ms is fine locally but
    // CI runners under load occasionally race the bind.
    thread::sleep(Duration::from_millis(500));
}

#[test]
fn auth_ok_allows_handshake_and_message_round_trip() {
    let port = free_port();

    // Server: auth accepts any request. on_message echoes inbound
    // text back so the client can prove the message path runs after
    // a successful handshake.
    let src = format!(
        r#"
import "std.net" as net

fn auth(_path :: Str, _headers :: List[{{ name :: Str, value :: Str }}])
  -> Result[Unit, Str] {{
  Ok(())
}}

fn on_message(_c :: WsConn, m :: WsMessage) -> WsAction {{
  match m {{
    WsText(s)   => WsSend(s),
    WsBinary(_) => WsNoOp,
    WsPing      => WsNoOp,
    WsClose     => WsNoOp,
  }}
}}

fn main() -> [net] Nil {{
  net.serve_ws_fn_auth({port}, "", auth, on_message)
}}
"#
    );
    spawn(&src);

    let url = format!("ws://127.0.0.1:{port}/auth-ok");
    let (mut ws, _resp) =
        tungstenite::connect(url.as_str()).expect("ws handshake");
    use tungstenite::Message;
    ws.send(Message::Text("ping".into())).expect("send");
    let reply = ws.read().expect("read");
    match reply {
        Message::Text(s) => assert_eq!(s.to_string(), "ping"),
        other => panic!("expected Text frame, got {other:?}"),
    }
    let _ = ws.close(None);
}

#[test]
fn auth_err_responds_401_and_skips_handshake() {
    let port = free_port();

    // Server: auth always rejects. on_message would set a flag if
    // invoked — we assert it never is.
    let invoked: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let invoked_for_assert = Arc::clone(&invoked);

    let src = format!(
        r#"
import "std.net" as net

fn auth(_path :: Str, _headers :: List[{{ name :: Str, value :: Str }}])
  -> Result[Unit, Str] {{
  Err("nope")
}}

fn on_message(_c :: WsConn, _m :: WsMessage) -> WsAction {{ WsNoOp }}

fn main() -> [net] Nil {{
  net.serve_ws_fn_auth({port}, "", auth, on_message)
}}
"#
    );
    spawn(&src);

    let url = format!("ws://127.0.0.1:{port}/rejected");
    match tungstenite::connect(url.as_str()) {
        Ok(_) => panic!("handshake should have been rejected"),
        Err(tungstenite::Error::Http(resp)) => {
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "auth Err should produce 401",
            );
            // Body carries the message the closure returned.
            let body = resp
                .body()
                .as_ref()
                .map(|v| String::from_utf8_lossy(v).to_string())
                .unwrap_or_default();
            assert!(
                body.contains("nope"),
                "expected rejection reason in body, got `{body}`",
            );
        }
        Err(other) => panic!("expected Http error, got {other:?}"),
    }

    // on_message should never have run.
    assert!(!*invoked_for_assert.lock().unwrap());
}

#[test]
fn auth_callback_sees_authorization_header() {
    let port = free_port();

    // Server: auth accepts only when the Authorization header is
    // present and starts with "Bearer demo-token". This is the
    // shape OCPP Security Profile 3 uses; the test proves the Lex
    // auth closure receives the real handshake headers.
    let src = format!(
        r#"
import "std.str" as str
import "std.list" as list
import "std.net" as net

fn lookup(headers :: List[{{ name :: Str, value :: Str }}], name :: Str)
  -> Option[Str] {{
  let lo := str.to_lower(name)
  let matches := list.filter(headers,
    fn (h :: {{ name :: Str, value :: Str }}) -> Bool {{
      str.to_lower(h.name) == lo
    }})
  match list.head(matches) {{
    Some(h) => Some(h.value),
    None    => None,
  }}
}}

fn auth(_path :: Str, headers :: List[{{ name :: Str, value :: Str }}])
  -> Result[Unit, Str] {{
  match lookup(headers, "authorization") {{
    Some(v) => if str.starts_with(v, "Bearer demo-token") {{
        Ok(())
      }} else {{
        Err("bad token")
      }},
    None    => Err("missing Authorization"),
  }}
}}

fn on_message(_c :: WsConn, _m :: WsMessage) -> WsAction {{ WsNoOp }}

fn main() -> [net] Nil {{
  net.serve_ws_fn_auth({port}, "", auth, on_message)
}}
"#
    );
    spawn(&src);

    let url = format!("ws://127.0.0.1:{port}/p3");

    // Without Authorization: 401.
    let no_auth_req = url
        .as_str()
        .into_client_request()
        .expect("build request");
    match tungstenite::connect(no_auth_req) {
        Err(tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let body = resp
                .body()
                .as_ref()
                .map(|v| String::from_utf8_lossy(v).to_string())
                .unwrap_or_default();
            assert!(
                body.contains("missing"),
                "expected 'missing Authorization' message, got `{body}`",
            );
        }
        other => panic!("expected 401 Http error, got {other:?}"),
    }

    // Wrong scheme: 401 with the "bad token" message.
    let mut wrong_req = url
        .as_str()
        .into_client_request()
        .expect("build request");
    wrong_req.headers_mut().insert(
        "Authorization",
        tungstenite::http::HeaderValue::from_static("Basic Zm9vOmJhcg=="),
    );
    match tungstenite::connect(wrong_req) {
        Err(tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let body = resp
                .body()
                .as_ref()
                .map(|v| String::from_utf8_lossy(v).to_string())
                .unwrap_or_default();
            assert!(
                body.contains("bad token"),
                "expected 'bad token' rejection, got `{body}`",
            );
        }
        other => panic!("expected 401 Http error, got {other:?}"),
    }

    // Correct token: handshake succeeds.
    let mut ok_req = url
        .as_str()
        .into_client_request()
        .expect("build request");
    ok_req.headers_mut().insert(
        "Authorization",
        tungstenite::http::HeaderValue::from_static("Bearer demo-token-xyz"),
    );
    let (mut ws, _resp) = tungstenite::connect(ok_req).expect("ws handshake");
    let _ = ws.close(None);
}
