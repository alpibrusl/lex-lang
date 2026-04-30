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
                    if let Err(e) = ws.send(Message::Text(msg)) {
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
    rec.insert("body".into(), Value::Str(body.to_string()));
    rec.insert("conn_id".into(), Value::Int(conn_id as i64));
    rec.insert("room".into(), Value::Str(room.to_string()));
    Value::Record(rec)
}
