//! WebSocket server + chat-broadcast registry.
//!
//! `net.serve_ws(port, on_message)` blocks on a TCP listener, upgrades
//! each incoming connection to WebSocket, and runs a per-connection
//! worker thread that polls both inbound (calls Lex's `on_message`)
//! and outbound (drains broadcasts from a channel into the socket).
//!
//! `chat.broadcast(room, body)` looks up every connection in `room`
//! and pushes `body` onto its outbound channel. `chat.send(conn_id,
//! body)` is the same but to a single connection.
//!
//! The registry is an `Arc<Mutex<…>>` because Lex's immutability means
//! shared mutable state has to live in the host runtime. Lex code
//! stays pure: it receives an event, returns Nil, and any side
//! effects go through `chat.*` which is gated by the policy.

// tungstenite's `accept_hdr` callback takes/returns a tungstenite
// `ErrorResponse` which is large; we only ever return Ok so the
// large-Err warning is noise.
#![allow(clippy::result_large_err)]

use crate::policy::Policy;
use indexmap::IndexMap;
use lex_bytecode::vm::Vm;
use lex_bytecode::{Program, Value};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Per-connection state held in the global registry.
struct Conn {
    room: String,
    /// Channel writer end. The connection's worker thread reads from
    /// the corresponding Receiver and writes each message to the
    /// WebSocket. Broadcasts push here.
    outbound: mpsc::Sender<String>,
}

/// Global chat registry. One per `net.serve_ws` invocation.
#[derive(Default)]
pub struct ChatRegistry {
    conns: Mutex<IndexMap<u64, Conn>>,
}

impl ChatRegistry {
    fn register(&self, room: String, outbound: mpsc::Sender<String>) -> u64 {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        self.conns.lock().unwrap().insert(id, Conn { room, outbound });
        id
    }
    fn unregister(&self, id: u64) {
        self.conns.lock().unwrap().shift_remove(&id);
    }
    fn broadcast(&self, room: &str, body: &str) {
        let conns = self.conns.lock().unwrap();
        for c in conns.values() {
            if c.room == room {
                let _ = c.outbound.send(body.to_string());
            }
        }
    }
    fn send_to(&self, id: u64, body: &str) -> bool {
        if let Some(c) = self.conns.lock().unwrap().get(&id) {
            let _ = c.outbound.send(body.to_string());
            true
        } else {
            false
        }
    }
}

/// `chat.broadcast(room, body)` — looked up at runtime by the
/// effect handler; called from inside the Lex VM.
pub fn chat_broadcast(reg: &Arc<ChatRegistry>, room: &str, body: &str) {
    reg.broadcast(room, body);
}

pub fn chat_send(reg: &Arc<ChatRegistry>, conn_id: u64, body: &str) -> bool {
    reg.send_to(conn_id, body)
}

/// Bind a WebSocket server. Blocks; returns Unit on shutdown (the
/// process is normally killed before that).
pub fn serve_ws(
    port: u16,
    handler_name: String,
    program: Arc<Program>,
    policy: Policy,
    registry: Arc<ChatRegistry>,
) -> Result<Value, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("net.serve_ws bind {port}: {e}"))?;
    eprintln!("net.serve_ws: listening on ws://127.0.0.1:{port}");
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => { eprintln!("net.serve_ws accept: {e}"); continue; }
        };
        let program = Arc::clone(&program);
        let policy = policy.clone();
        let handler_name = handler_name.clone();
        let registry = Arc::clone(&registry);
        thread::spawn(move || {
            if let Err(e) = handle_connection(stream, program, policy, handler_name, registry) {
                eprintln!("net.serve_ws connection error: {e}");
            }
        });
    }
    Ok(Value::Unit)
}

fn handle_connection(
    stream: std::net::TcpStream,
    program: Arc<Program>,
    policy: Policy,
    handler_name: String,
    registry: Arc<ChatRegistry>,
) -> Result<(), String> {
    use tungstenite::{accept_hdr, handshake::server::{Request, Response}};

    // Capture the request path during the handshake — used as the room name.
    let mut path = String::new();
    let path_ref = &mut path;
    let mut ws = accept_hdr(stream, |req: &Request, resp: Response| {
        *path_ref = req.uri().path().to_string();
        Ok(resp)
    }).map_err(|e| format!("ws handshake: {e}"))?;

    let room = path.trim_start_matches('/').to_string();

    // Outbound channel: broadcast/send pushes here, this thread writes
    // each message into the WebSocket.
    let (tx, rx) = mpsc::channel::<String>();
    let conn_id = registry.register(room.clone(), tx);

    // Make WS reads non-blocking-ish so the same thread can also drain
    // the outbound channel. tungstenite reads through the underlying
    // TcpStream; setting a short read timeout lets us multiplex.
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(50)));

    let result = run_loop(&mut ws, &rx, conn_id, &room, &program, &policy, &handler_name, &registry);
    registry.unregister(conn_id);
    let _ = ws.close(None);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
    rx: &mpsc::Receiver<String>,
    conn_id: u64,
    room: &str,
    program: &Arc<Program>,
    policy: &Policy,
    handler_name: &str,
    registry: &Arc<ChatRegistry>,
) -> Result<(), String> {
    use tungstenite::Message;
    use std::io::ErrorKind;
    loop {
        // 1) Try to read one inbound message. WouldBlock = no data yet.
        match ws.read() {
            Ok(Message::Text(body)) => {
                let ev = build_ws_event(conn_id, room, &body);
                let handler = crate::handler::DefaultHandler::new(policy.clone())
                    .with_program(Arc::clone(program))
                    .with_chat_registry(Arc::clone(registry));
                let mut vm = Vm::with_handler(program, Box::new(handler));
                if let Err(e) = vm.call(handler_name, vec![ev]) {
                    eprintln!("on_message {conn_id}: {e}");
                }
            }
            Ok(Message::Binary(_)) => { /* binary frames ignored in v1 */ }
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => break,
            Ok(_) => {} // ping/pong/frame
            Err(tungstenite::Error::Io(ref e)) if e.kind() == ErrorKind::WouldBlock
                || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("ws read: {e}")),
        }
        // 2) Drain outbound channel. Doesn't block.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if let Err(e) = ws.send(Message::Text(msg.into())) {
                        return Err(format!("ws send: {e}"));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }
    Ok(())
}

fn build_ws_event(conn_id: u64, room: &str, body: &str) -> Value {
    let mut rec = IndexMap::new();
    rec.insert("body".into(), Value::Str(body.into()));
    rec.insert("conn_id".into(), Value::Int(conn_id as i64));
    rec.insert("room".into(), Value::Str(room.into()));
    Value::Record(rec)
}

// ── Closure-based WebSocket server (#359) ────────────────────────────────────

/// Build a `WsConn` record value for the typed closure-based handler.
fn build_ws_conn(conn_id: u64, path: &str, subprotocol: &str) -> Value {
    let mut rec = IndexMap::new();
    rec.insert("id".into(), Value::Str(conn_id.to_string().into()));
    rec.insert("path".into(), Value::Str(path.into()));
    rec.insert("subprotocol".into(), Value::Str(subprotocol.into()));
    Value::Record(rec)
}

/// Build a `WsMessage` variant value.
fn build_ws_message_text(body: &str) -> Value {
    Value::Variant { name: "WsText".into(), args: vec![Value::Str(body.into())] }
}

fn build_ws_message_close() -> Value {
    Value::Variant { name: "WsClose".into(), args: vec![] }
}

fn build_ws_message_ping() -> Value {
    Value::Variant { name: "WsPing".into(), args: vec![] }
}

fn build_ws_message_binary(payload: &[u8]) -> Value {
    let bytes = payload.iter().map(|b| Value::Int(*b as i64)).collect();
    Value::Variant { name: "WsBinary".into(), args: vec![Value::List(bytes)] }
}

/// Interpret a `WsAction` variant and send the appropriate frame.
/// Generic over the stream so this serves both the plaintext-only
/// server path (`TcpStream`) and the dial path that may sit on top
/// of a TLS-wrapped stream (`MaybeTlsStream<TcpStream>`).
fn apply_ws_action<S: std::io::Read + std::io::Write>(
    action: &Value,
    ws: &mut tungstenite::WebSocket<S>,
) -> Result<(), String> {
    use tungstenite::Message;
    match action {
        Value::Variant { name, args } if name == "WsSend" => {
            let text = match args.first() {
                Some(Value::Str(s)) => s.clone(),
                _ => return Err("WsSend payload must be Str".into()),
            };
            ws.send(Message::Text(text.to_string().into()))
                .map_err(|e| format!("ws send: {e}"))
        }
        Value::Variant { name, args } if name == "WsSendBinary" => {
            let bytes: Vec<u8> = match args.first() {
                Some(Value::List(elems)) => elems
                    .iter()
                    .map(|v| match v {
                        Value::Int(n) => Ok(*n as u8),
                        _ => Err("WsSendBinary payload must be List[Int]".into()),
                    })
                    .collect::<Result<Vec<_>, String>>()?,
                _ => return Err("WsSendBinary payload must be List[Int]".into()),
            };
            ws.send(Message::Binary(bytes.into()))
                .map_err(|e| format!("ws send binary: {e}"))
        }
        Value::Variant { name, .. } if name == "WsNoOp" => Ok(()),
        other => Err(format!("unexpected WsAction: {other:?}")),
    }
}

/// Closure-based WebSocket server. Accepts a `Value::Closure` as the handler.
pub fn serve_ws_fn(
    port: u16,
    subprotocol: String,
    closure: Value,
    program: Arc<Program>,
    policy: Policy,
    registry: Arc<ChatRegistry>,
) -> Result<Value, String> {
    // Fail fast: a configured subprotocol that can't be a valid HTTP
    // header value would silently break every handshake later (the
    // accept_hdr callback's `HeaderValue::from_str` would always
    // return Err). Reject at startup with a clear message instead.
    if !subprotocol.is_empty() {
        if let Err(e) =
            tungstenite::http::HeaderValue::from_str(&subprotocol)
        {
            return Err(format!(
                "net.serve_ws_fn: subprotocol {subprotocol:?} is not a valid \
                 HTTP header value: {e}"
            ));
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("net.serve_ws_fn bind {port}: {e}"))?;
    eprintln!("net.serve_ws_fn: listening on ws://127.0.0.1:{port}");
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => { eprintln!("net.serve_ws_fn accept: {e}"); continue; }
        };
        let program = Arc::clone(&program);
        let policy = policy.clone();
        let closure = closure.clone();
        let subprotocol = subprotocol.clone();
        let registry = Arc::clone(&registry);
        thread::spawn(move || {
            if let Err(e) = handle_connection_fn(
                stream, program, policy, closure, subprotocol, registry,
            ) {
                eprintln!("net.serve_ws_fn connection error: {e}");
            }
        });
    }
    Ok(Value::Unit)
}

fn handle_connection_fn(
    stream: std::net::TcpStream,
    program: Arc<Program>,
    policy: Policy,
    closure: Value,
    subprotocol: String,
    registry: Arc<ChatRegistry>,
) -> Result<(), String> {
    use tungstenite::{accept_hdr, handshake::server::{Request, Response}};
    use tungstenite::http::HeaderValue;

    let mut path = String::new();
    let path_ref = &mut path;
    let subproto_for_handshake = subprotocol.clone();
    let mut ws = accept_hdr(stream, |req: &Request, mut resp: Response| {
        *path_ref = req.uri().path().to_string();
        // Echo the negotiated subprotocol back so strict clients
        // (RFC 6455 §4.1, tungstenite ≥ 0.20) accept the handshake.
        // Only echo when (a) the server has a non-empty subprotocol,
        // (b) the client offered subprotocols, and (c) the server's
        // configured value is among the client's offers. RFC 6455
        // §4.1 mandates the server MUST select one of the client's
        // offers — echoing a value the client did not offer would
        // be spec-noncompliant and trip strict clients. The empty
        // server-side configuration → no echo branch matches the
        // dial_ws contract (empty subprotocol = no header).
        if !subproto_for_handshake.is_empty() {
            if let Some(offered) = req.headers().get("Sec-WebSocket-Protocol") {
                if let Ok(offered_str) = offered.to_str() {
                    let client_offers =
                        offered_str.split(',').map(|p| p.trim());
                    if client_offers
                        .clone()
                        .any(|p| p == subproto_for_handshake)
                    {
                        // from_str cannot fail here: serve_ws_fn
                        // validated `subprotocol` upfront. Belt-and-
                        // braces: silently skip if it somehow does.
                        if let Ok(h) =
                            HeaderValue::from_str(&subproto_for_handshake)
                        {
                            resp.headers_mut()
                                .insert("Sec-WebSocket-Protocol", h);
                        }
                    }
                }
            }
        }
        Ok(resp)
    }).map_err(|e| format!("ws handshake: {e}"))?;

    let (tx, rx) = mpsc::channel::<String>();
    let conn_id = registry.register(path.trim_start_matches('/').to_string(), tx);
    let _ = ws.get_mut().set_read_timeout(Some(Duration::from_millis(50)));

    let result = run_loop_fn(
        &mut ws, &rx, conn_id, &path, &subprotocol,
        &program, &policy, &closure, &registry,
    );
    registry.unregister(conn_id);
    let _ = ws.close(None);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_loop_fn(
    ws: &mut tungstenite::WebSocket<std::net::TcpStream>,
    rx: &mpsc::Receiver<String>,
    conn_id: u64,
    path: &str,
    subprotocol: &str,
    program: &Arc<Program>,
    policy: &Policy,
    closure: &Value,
    registry: &Arc<ChatRegistry>,
) -> Result<(), String> {
    use tungstenite::Message;
    use std::io::ErrorKind;

    let ws_conn = build_ws_conn(conn_id, path, subprotocol);

    loop {
        let ws_msg = match ws.read() {
            Ok(Message::Text(body)) => Some(build_ws_message_text(&body)),
            Ok(Message::Binary(_)) => None,
            Ok(Message::Ping(_)) => Some(build_ws_message_ping()),
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                // Notify handler then exit.
                let handler = crate::handler::DefaultHandler::new(policy.clone())
                    .with_program(Arc::clone(program))
                    .with_chat_registry(Arc::clone(registry));
                let mut vm = Vm::with_handler(program, Box::new(handler));
                let _ = vm.invoke_closure_value(
                    closure.clone(),
                    vec![ws_conn.clone(), build_ws_message_close()],
                );
                break;
            }
            Ok(_) => None, // pong / frame
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => None,
            Err(e) => return Err(format!("ws read: {e}")),
        };

        if let Some(msg) = ws_msg {
            let handler = crate::handler::DefaultHandler::new(policy.clone())
                .with_program(Arc::clone(program))
                .with_chat_registry(Arc::clone(registry));
            let mut vm = Vm::with_handler(program, Box::new(handler));
            match vm.invoke_closure_value(closure.clone(), vec![ws_conn.clone(), msg]) {
                Ok(action) => {
                    if let Err(e) = apply_ws_action(&action, ws) {
                        eprintln!("ws action {conn_id}: {e}");
                    }
                }
                Err(e) => eprintln!("ws handler {conn_id}: {e}"),
            }
        }

        // Drain broadcast/send outbound channel.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if let Err(e) = ws.send(Message::Text(msg.into())) {
                        return Err(format!("ws send: {e}"));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }
    Ok(())
}

// ── Closure-based WebSocket client (#390) ────────────────────────────────────
//
// Inverse of `serve_ws_fn`: open a connection to a remote WS server and
// run two Lex callbacks against it.
//
// - `on_open : () -> [E] WsAction` is invoked once after the handshake
//   completes. The returned `WsAction` (typically `WsSend(boot_frame)`)
//   is applied to the socket immediately. This is the hook for
//   protocols like OCPP where the client sends a `BootNotification`
//   the moment it connects.
// - `on_message : (WsMessage) -> [E] WsAction` is invoked for every
//   inbound frame. Same `WsAction` semantics as the server-side
//   handler. A `WsClose` message is delivered once before the loop
//   exits so handlers can run shutdown logic.
//
// Multi-frame sends from `on_open` (e.g. a charger that wants to
// also kick off a heartbeat scheduler at connect-time) aren't
// expressible in v1 — the issue's `send :: (Str) -> [net]
// Result[Unit, Str]` closure would let users push outbound frames
// from arbitrary `[net]` code, but that requires representing
// Rust-native closures as Lex `Value`s, which is a separate
// runtime change. v1 covers the BootNotification + reactive reply
// pattern that motivates the issue.

fn build_dial_result(ok: Result<(), String>) -> Value {
    match ok {
        Ok(()) => Value::Variant {
            name: "Ok".into(),
            args: vec![Value::Unit],
        },
        Err(msg) => Value::Variant {
            name: "Err".into(),
            args: vec![Value::Str(msg.into())],
        },
    }
}

/// `net.dial_ws(url, subprotocol, on_open, on_message) -> [net, E]
/// Result[Unit, Str]`. Blocks for the lifetime of the connection;
/// returns `Ok(())` on a clean close from the server, `Err(reason)`
/// on dial failure, handshake failure, read error, or write error.
pub fn dial_ws(
    url: String,
    subprotocol: String,
    on_open: Value,
    on_message: Value,
    program: Arc<Program>,
    policy: Policy,
) -> Result<Value, String> {
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::HeaderValue;

    // Build the request — when `subprotocol` is non-empty, attach the
    // Sec-WebSocket-Protocol header so the server's accept-handler
    // can match on it. Empty subprotocol → header omitted (the same
    // contract as `serve_ws_fn`'s subprotocol arg).
    //
    // Caller-controlled inputs (URL syntax, subprotocol header value)
    // surface as a Lex `Err(reason)`, not a Rust panic / handler
    // error, so `match net.dial_ws(...) { Err(_) => ..., Ok(_) => ... }`
    // works at the Lex level.
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            return Ok(build_dial_result(Err(format!(
                "net.dial_ws: bad URL `{url}`: {e}"
            ))));
        }
    };
    if !subprotocol.is_empty() {
        let header = match HeaderValue::from_str(&subprotocol) {
            Ok(h) => h,
            Err(e) => {
                return Ok(build_dial_result(Err(format!(
                    "net.dial_ws: invalid subprotocol `{subprotocol}`: {e}"
                ))));
            }
        };
        req.headers_mut().insert("Sec-WebSocket-Protocol", header);
    }

    let (mut ws, _resp) = match tungstenite::connect(req) {
        Ok(pair) => pair,
        Err(e) => {
            return Ok(build_dial_result(Err(format!(
                "net.dial_ws: connect to `{url}`: {e}"
            ))));
        }
    };

    // Non-blocking-ish reads so we don't tie up the thread on an idle
    // socket, mirroring the server's read-timeout multiplexing.
    if let Some(stream) = stream_for(&mut ws) {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    }

    // 1. Fire on_open once and apply its action.
    {
        let handler = crate::handler::DefaultHandler::new(policy.clone())
            .with_program(Arc::clone(&program));
        let mut vm = Vm::with_handler(&program, Box::new(handler));
        match vm.invoke_closure_value(on_open.clone(), vec![]) {
            Ok(action) => {
                if let Err(e) = apply_ws_action(&action, &mut ws) {
                    return Ok(build_dial_result(Err(format!(
                        "net.dial_ws: on_open action: {e}"
                    ))));
                }
            }
            Err(e) => {
                return Ok(build_dial_result(Err(format!(
                    "net.dial_ws: on_open: {e}"
                ))));
            }
        }
    }

    // 2. Run the read loop, dispatching each inbound frame to on_message.
    let loop_result = dial_run_loop(&mut ws, &on_message, &program, &policy);
    let _ = ws.close(None);
    Ok(build_dial_result(loop_result))
}

/// Pull the underlying TCP stream out of a `MaybeTlsStream` so we can
/// set a read timeout. For plaintext connections this is the
/// `TcpStream` directly; for `rustls`-wrapped streams it's the inner
/// socket. Returns `None` if the wrapping is some other variant —
/// in that case we just skip the timeout and rely on blocking reads.
fn stream_for(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
) -> Option<&mut std::net::TcpStream> {
    use tungstenite::stream::MaybeTlsStream;
    match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Some(s),
        MaybeTlsStream::Rustls(s) => Some(s.get_mut()),
        _ => None,
    }
}

fn dial_run_loop(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    on_message: &Value,
    program: &Arc<Program>,
    policy: &Policy,
) -> Result<(), String> {
    use std::io::ErrorKind;
    use tungstenite::Message;

    loop {
        let ws_msg = match ws.read() {
            Ok(Message::Text(body)) => Some(build_ws_message_text(&body)),
            Ok(Message::Binary(payload)) => Some(build_ws_message_binary(&payload)),
            Ok(Message::Ping(_)) => Some(build_ws_message_ping()),
            Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                // Deliver WsClose so the handler can do shutdown work.
                let handler = crate::handler::DefaultHandler::new(policy.clone())
                    .with_program(Arc::clone(program));
                let mut vm = Vm::with_handler(program, Box::new(handler));
                let _ = vm.invoke_closure_value(
                    on_message.clone(),
                    vec![build_ws_message_close()],
                );
                return Ok(());
            }
            Ok(_) => None, // pong / raw frame
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
            {
                None
            }
            Err(e) => return Err(format!("net.dial_ws: read: {e}")),
        };

        if let Some(msg) = ws_msg {
            let handler = crate::handler::DefaultHandler::new(policy.clone())
                .with_program(Arc::clone(program));
            let mut vm = Vm::with_handler(program, Box::new(handler));
            match vm.invoke_closure_value(on_message.clone(), vec![msg]) {
                Ok(action) => {
                    if let Err(e) = apply_ws_action(&action, ws) {
                        return Err(format!("net.dial_ws: action: {e}"));
                    }
                }
                Err(e) => return Err(format!("net.dial_ws: on_message: {e}")),
            }
        }
    }
}
