//! End-to-end test for `net.serve_ws_fn_actor` (#459).
//!
//! The contract is: when a WebSocket client dials in, the runtime
//! calls the user's `name_of` closure with a `WsConn`, registers a
//! native bridge actor under the returned name in `conc_registry`,
//! and any frame `conc.tell`'d to that actor from a non-WS context
//! is written to the socket as a text frame. Unregistration on
//! disconnect is verified separately.
//!
//! Server thread leaks (`serve_ws_fn_actor` blocks forever — same
//! limitation as `serve_ws_fn`); cargo reaps it on process exit.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, conc_registry, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

fn spawn_server(src: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string(), "concurrent".to_string(), "io".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    // Give the server a moment to bind.
    thread::sleep(Duration::from_millis(400));
}

fn server_src(port: u16) -> String {
    // `name_of` derives the registry name from the request path:
    // `/ws/<id>` → `ws:<id>`. on_message just echoes back so an
    // inbound frame round-trip is observable.
    format!(
        r#"
import "std.net"  as net
import "std.str"  as str

fn name_of(conn :: WsConn) -> Str {{
  # path looks like "/ws/cp001"; strip the prefix.
  match str.strip_prefix(conn.path, "/ws/") {{
    Some(id) => str.concat("ws:", id),
    None     => "",
  }}
}}

fn on_message(_c :: WsConn, msg :: WsMessage) -> WsAction {{
  match msg {{
    WsText(body) => WsSend(str.concat("echo:", body)),
    _            => WsNoOp,
  }}
}}

fn main() -> [net, concurrent] Nil {{
  net.serve_ws_fn_actor({port}, "", name_of, on_message)
}}
"#
    )
}

/// Inspect `conc_registry::lookup` from the test thread, then run a
/// short Lex program that fishes the actor back out and `conc.tell`s
/// it. Returns the registered actor handle (still held by the global
/// registry).
fn tell_via_lex_program(name: &str, body: &str) -> bool {
    let src = format!(
        r#"
import "std.conc" as conc

fn tell_if_present(name :: Str, body :: Str) -> [concurrent] Bool {{
  match conc.lookup(name) {{
    None        => false,
    Some(actor) => {{
      let _ := conc.tell(actor, body)
      true
    }},
  }}
}}
"#
    );
    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors in tell helper: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let mut policy = Policy::pure();
    policy.allow_effects = ["concurrent".to_string()].into_iter().collect();
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let v = vm
        .call(
            "tell_if_present",
            vec![Value::Str(name.into()), Value::Str(body.into())],
        )
        .expect("vm tell");
    matches!(v, Value::Bool(true))
}

#[test]
fn outbound_tell_reaches_the_socket() {
    let _ = conc_registry::_reset_for_tests();
    let port = free_port();
    spawn_server(&server_src(port));

    // Dial in. The path determines the registry name via `name_of`.
    let url = format!("ws://127.0.0.1:{port}/ws/cp001");
    let req = url.as_str().into_client_request().expect("request");
    let (mut client, _resp) = tungstenite::connect(req).expect("ws connect");

    // Wait for the server to finish registration.
    let mut found = false;
    for _ in 0..40 {
        if conc_registry::lookup("ws:cp001").is_some() {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "actor never appeared in conc_registry");

    // Push a frame from a non-WS path.
    let sent = tell_via_lex_program("ws:cp001", "hello-from-non-ws");
    assert!(sent, "tell_if_present returned false");

    // Read the frame off the wire.
    use tungstenite::Message;
    use tungstenite::stream::MaybeTlsStream;
    if let MaybeTlsStream::Plain(s) = client.get_mut() {
        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    }
    let msg = client.read().expect("read");
    match msg {
        Message::Text(t) => assert_eq!(t.as_str(), "hello-from-non-ws"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[test]
fn name_of_empty_string_skips_registration() {
    let _ = conc_registry::_reset_for_tests();
    let port = free_port();
    // name_of returns "" — the connection should not appear in
    // conc_registry, but on_message should still run.
    let src = format!(
        r#"
import "std.net" as net
import "std.str" as str

fn name_of(_c :: WsConn) -> Str {{ "" }}

fn on_message(_c :: WsConn, msg :: WsMessage) -> WsAction {{
  match msg {{
    WsText(body) => WsSend(str.concat("seen:", body)),
    _            => WsNoOp,
  }}
}}

fn main() -> [net, concurrent] Nil {{
  net.serve_ws_fn_actor({port}, "", name_of, on_message)
}}
"#
    );
    spawn_server(&src);

    let url = format!("ws://127.0.0.1:{port}/anywhere");
    let req = url.as_str().into_client_request().expect("request");
    let (mut client, _resp) = tungstenite::connect(req).expect("ws connect");

    // Give the server a beat to settle, then probe the registry.
    thread::sleep(Duration::from_millis(200));
    assert!(
        conc_registry::registered().is_empty(),
        "registry should be empty when name_of returns \"\""
    );

    // Inbound still works — the on_message handler echoes back with a "seen:" prefix.
    use tungstenite::Message;
    client
        .send(Message::Text("ping".to_string().into()))
        .expect("send");
    if let MaybeTlsStream::Plain(s) = client.get_mut() {
        let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    }
    let msg = client.read().expect("read");
    match msg {
        Message::Text(t) => assert_eq!(t.as_str(), "seen:ping"),
        other => panic!("expected text frame, got {other:?}"),
    }
}

#[test]
fn unregister_on_disconnect_clears_the_name() {
    let _ = conc_registry::_reset_for_tests();
    let port = free_port();
    spawn_server(&server_src(port));

    let url = format!("ws://127.0.0.1:{port}/ws/cp002");
    let req = url.as_str().into_client_request().expect("request");
    let (mut client, _resp) = tungstenite::connect(req).expect("ws connect");

    // Wait for register.
    let mut found = false;
    for _ in 0..40 {
        if conc_registry::lookup("ws:cp002").is_some() {
            found = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "actor never appeared after dial");

    // Close from the client side. The server's read loop sees a
    // Close frame, exits, and unregisters the name from conc_registry.
    client.close(None).expect("close");
    drop(client);

    // Give the server's run loop a chance to observe the close +
    // unregister. 1s is generous given the 50ms read poll interval.
    let mut cleared = false;
    for _ in 0..40 {
        if conc_registry::lookup("ws:cp002").is_none() {
            cleared = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(cleared, "name should be unregistered after socket close");
}
