//! Integration tests for WebSocket multi-user chat.
//!
//! Spawns the chat server in a background thread, connects N clients,
//! has one send a message, asserts every other client in the same
//! room receives it. Different rooms are isolated.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tungstenite::{connect, Message};

fn spawn_chat_server(src: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string(), "chat".to_string()].into_iter().collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(200));
}

const CHAT_SRC_TEMPLATE: &str = r#"
import "std.net" as net
import "std.chat" as chat
import "std.str" as str
import "std.int" as int

type Ev = { body :: Str, conn_id :: Int, room :: Str }

fn on_message(ev :: Ev) -> [chat] Nil {
  let prefix := str.concat("[", str.concat(int.to_str(ev.conn_id), "] "))
  chat.broadcast(ev.room, str.concat(prefix, ev.body))
}

fn main() -> [chat, net] Nil { net.serve_ws(__PORT__, "on_message") }
"#;

fn chat_src(port: u16) -> String {
    CHAT_SRC_TEMPLATE.replace("__PORT__", &port.to_string())
}

fn dial(port: u16, room: &str) -> tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>> {
    let url = format!("ws://127.0.0.1:{port}/{room}");
    let (ws, _resp) = connect(url).expect("ws connect");
    ws
}

fn read_text(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    timeout: Duration,
) -> Option<String> {
    // We can't change the timeout on an existing read; rely on the
    // test calling with patience. tungstenite's `read` blocks; for
    // bounded waits in tests we set the underlying TcpStream to a
    // short read timeout once after connect.
    let _ = timeout;
    match ws.read() {
        Ok(Message::Text(s)) => Some(s),
        _ => None,
    }
}

fn set_read_timeout(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    d: Duration,
) {
    if let tungstenite::stream::MaybeTlsStream::Plain(ref mut tcp) = ws.get_mut() {
        let _ = tcp.set_read_timeout(Some(d));
    }
}

#[test]
fn broadcast_reaches_other_clients_in_same_room() {
    let port = 19101;
    spawn_chat_server(&chat_src(port));

    // Two clients join the same room.
    let mut alice = dial(port, "lobby");
    let mut bob = dial(port, "lobby");
    set_read_timeout(&mut alice, Duration::from_secs(2));
    set_read_timeout(&mut bob, Duration::from_secs(2));

    // alice sends; both alice and bob should receive (server echoes
    // to all in the room, including the sender).
    alice.send(Message::Text("hello!".into())).unwrap();

    let msg_a = read_text(&mut alice, Duration::from_secs(2)).expect("alice reads");
    let msg_b = read_text(&mut bob, Duration::from_secs(2)).expect("bob reads");
    assert!(msg_a.ends_with(" hello!"), "alice got: {msg_a}");
    assert!(msg_b.ends_with(" hello!"), "bob got: {msg_b}");

    // The two prefixes should match — same sender, same conn_id.
    let prefix_a = msg_a.split_once(' ').unwrap().0;
    let prefix_b = msg_b.split_once(' ').unwrap().0;
    assert_eq!(prefix_a, prefix_b, "same sender → same prefix");
}

#[test]
fn rooms_are_isolated() {
    let port = 19102;
    spawn_chat_server(&chat_src(port));

    let mut a_lobby = dial(port, "lobby");
    let mut a_general = dial(port, "general");
    set_read_timeout(&mut a_lobby, Duration::from_millis(500));
    set_read_timeout(&mut a_general, Duration::from_millis(500));

    a_lobby.send(Message::Text("for-lobby-only".into())).unwrap();

    // a_lobby should receive its own broadcast.
    let lobby_msg = read_text(&mut a_lobby, Duration::from_secs(2)).expect("lobby reads");
    assert!(lobby_msg.contains("for-lobby-only"), "lobby got: {lobby_msg}");

    // a_general should time out (no broadcast crossed rooms).
    let crossed = read_text(&mut a_general, Duration::from_millis(500));
    assert!(crossed.is_none(), "general accidentally received: {crossed:?}");
}

#[test]
fn many_clients_fan_out() {
    let port = 19103;
    spawn_chat_server(&chat_src(port));

    const N: usize = 8;
    let mut clients = Vec::with_capacity(N);
    for _ in 0..N {
        let mut ws = dial(port, "room1");
        set_read_timeout(&mut ws, Duration::from_secs(2));
        clients.push(ws);
    }
    // First client sends a message.
    clients[0].send(Message::Text("ping".into())).unwrap();

    // Every client should see the broadcast (sender included).
    let mut ok = 0;
    for (i, c) in clients.iter_mut().enumerate() {
        match read_text(c, Duration::from_secs(2)) {
            Some(s) if s.contains("ping") => ok += 1,
            other => eprintln!("client {i} got {other:?}"),
        }
    }
    assert_eq!(ok, N, "{ok}/{N} clients received the broadcast");
}
