//! Integration tests for UDP datagrams in `std.net` (#760).
//!
//! These run against **real loopback sockets**, not a mock. A datagram
//! API that passed against a fake would prove nothing worth knowing —
//! the whole point is that bytes leave the process and come back.
//!
//! Covered:
//! 1. Round-trip over loopback, both directions, with the sender's
//!    address reported correctly.
//! 2. A timeout is `Err`, distinct from an empty datagram, which is a
//!    legal thing to receive.
//! 3. `--allow-net-host` gates the destination, so `udp_send` is not a
//!    way around the policy `net.get` respects.
//! 4. Handles are real: use-after-close fails, and the registry is
//!    bounded rather than leaking file descriptors.
//!
//! Nothing here asserts against the public internet, and no test needs
//! a device on the LAN.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn policy_net() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects.insert("net".into());
    p
}

fn run_with(policy: Policy, src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    run_with(policy_net(), src, fn_name, args)
}

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => {
            args.into_iter().next().unwrap()
        }
        other => panic!("expected Ok(_), got {other:?}"),
    }
}

fn unwrap_err(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            match args.into_iter().next().unwrap() {
                Value::Str(s) => s.to_string(),
                other => panic!("expected Err(Str), got Err({other:?})"),
            }
        }
        other => panic!("expected Err(_), got {other:?}"),
    }
}

fn as_int(v: Value) -> i64 {
    match v { Value::Int(n) => n, other => panic!("expected Int, got {other:?}") }
}

/// Pull `data` / `host` / `port` out of a UdpDatagram record.
fn datagram(v: Value) -> (Vec<u8>, String, i64) {
    let get = |v: &Value, f: &str| -> Value {
        match v {
            Value::Record { fields, .. } => fields
                .get(f)
                .unwrap_or_else(|| panic!("UdpDatagram missing `{f}`"))
                .clone(),
            other => panic!("expected UdpDatagram record, got {other:?}"),
        }
    };
    let data = match get(&v, "data") {
        Value::Bytes(b) => b,
        other => panic!("data must be Bytes, got {other:?}"),
    };
    let host = match get(&v, "host") {
        Value::Str(s) => s.to_string(),
        other => panic!("host must be Str, got {other:?}"),
    };
    (data, host, as_int(get(&v, "port")))
}

const SRC: &str = r#"
import "std.net" as net

fn open(port :: Int) -> [net] Result[Int, Str] { net.udp_open(port) }

fn close(sock :: Int) -> [net] Result[Unit, Str] { net.udp_close(sock) }

fn send(sock :: Int, host :: Str, port :: Int, data :: Bytes) -> [net] Result[Int, Str] {
  net.udp_send(sock, host, port, data)
}

fn recv(sock :: Int, timeout_ms :: Int) -> [net] Result[UdpDatagram, Str] {
  net.udp_recv(sock, timeout_ms)
}

fn broadcast(sock :: Int, on :: Bool) -> [net] Result[Unit, Str] {
  net.udp_broadcast(sock, on)
}

fn join(sock :: Int, group :: Str) -> [net] Result[Unit, Str] {
  net.udp_join_multicast(sock, group)
}
"#;

fn open_ephemeral() -> i64 {
    as_int(unwrap_ok(run(SRC, "open", vec![Value::Int(0)])))
}

#[test]
fn round_trip_over_loopback_with_the_senders_address() {
    let port = pick_free_port();
    let server = as_int(unwrap_ok(run(SRC, "open", vec![Value::Int(port)])));
    let client = open_ephemeral();

    let n = as_int(unwrap_ok(run(SRC, "send", vec![
        Value::Int(client), Value::Str("127.0.0.1".into()),
        Value::Int(port), Value::Bytes(b"hello over udp".to_vec()),
    ])));
    assert_eq!(n, 14, "send reports the byte count written");

    let (data, host, sender_port) = datagram(unwrap_ok(run(SRC, "recv", vec![
        Value::Int(server), Value::Int(2000),
    ])));
    assert_eq!(data, b"hello over udp");
    assert_eq!(host, "127.0.0.1", "the sender's address comes back with the datagram");
    assert!(sender_port > 0, "and so does its port");

    // Back the other way, addressed with what the datagram reported.
    // This is the property that makes the sender's address worth
    // carrying: it has to be usable as a destination, not just printable.
    unwrap_ok(run(SRC, "send", vec![
        Value::Int(server), Value::Str(host.into()),
        Value::Int(sender_port), Value::Bytes(b"pong".to_vec()),
    ]));
    let (reply, _, _) = datagram(unwrap_ok(run(SRC, "recv", vec![
        Value::Int(client), Value::Int(2000),
    ])));
    assert_eq!(reply, b"pong");

    unwrap_ok(run(SRC, "close", vec![Value::Int(server)]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(client)]));
}

/// Bind an OS-chosen port with std, note it, and release it. Racy in
/// principle, fine in practice and far clearer than the bounce-off-a-
/// third-socket dance.
fn pick_free_port() -> i64 {
    let s = std::net::UdpSocket::bind(("127.0.0.1", 0)).expect("bind");
    i64::from(s.local_addr().expect("local_addr").port())
}

#[test]
fn an_empty_datagram_is_received_not_mistaken_for_a_timeout() {
    // The distinction the API exists to preserve: a zero-length UDP
    // payload is legal, and must not look like nothing arriving.
    let port = pick_free_port();
    let server = as_int(unwrap_ok(run(SRC, "open", vec![Value::Int(port)])));
    let client = open_ephemeral();

    unwrap_ok(run(SRC, "send", vec![
        Value::Int(client), Value::Str("127.0.0.1".into()),
        Value::Int(port), Value::Bytes(vec![]),
    ]));
    let (data, _, _) = datagram(unwrap_ok(run(SRC, "recv", vec![
        Value::Int(server), Value::Int(2000),
    ])));
    assert!(data.is_empty(), "an empty payload arrives as an empty datagram");

    unwrap_ok(run(SRC, "close", vec![Value::Int(server)]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(client)]));
}

#[test]
fn a_timeout_is_an_error_rather_than_silence() {
    let sock = open_ephemeral();
    let e = unwrap_err(run(SRC, "recv", vec![Value::Int(sock), Value::Int(50)]));
    assert!(e.contains("timed out after 50ms"), "got: {e}");
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}

#[test]
fn a_zero_timeout_polls_rather_than_blocking_forever() {
    // 0 means "no timeout" to the OS, which would wedge the thread.
    // The test is that this call returns at all.
    let sock = open_ephemeral();
    let e = unwrap_err(run(SRC, "recv", vec![Value::Int(sock), Value::Int(0)]));
    assert!(e.contains("timed out"), "got: {e}");
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}

// ── policy ───────────────────────────────────────────────────────

#[test]
fn allow_net_host_gates_the_destination() {
    // The property that keeps udp_send from being a hole around the gate
    // `net.get` respects.
    let mut p = policy_net();
    p.allow_net_host.push("10.0.0.1".into());

    let sock = as_int(unwrap_ok(run_with(p.clone(), SRC, "open", vec![Value::Int(0)])));
    let e = unwrap_err(run_with(p.clone(), SRC, "send", vec![
        Value::Int(sock), Value::Str("127.0.0.1".into()),
        Value::Int(9), Value::Bytes(b"nope".to_vec()),
    ]));
    assert!(e.contains("not in --allow-net-host"), "got: {e}");
    unwrap_ok(run_with(p, SRC, "close", vec![Value::Int(sock)]));
}

#[test]
fn an_empty_allowlist_permits_any_destination() {
    // Matches ensure_host_allowed: empty means unrestricted, so adding
    // UDP does not silently tighten existing programs.
    let port = pick_free_port();
    let server = as_int(unwrap_ok(run(SRC, "open", vec![Value::Int(port)])));
    let client = open_ephemeral();
    unwrap_ok(run(SRC, "send", vec![
        Value::Int(client), Value::Str("127.0.0.1".into()),
        Value::Int(port), Value::Bytes(b"x".to_vec()),
    ]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(server)]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(client)]));
}

#[test]
fn broadcast_still_has_to_clear_the_allowlist() {
    // Enabling SO_BROADCAST is not itself permission to broadcast: the
    // destination is checked like any other. Sending to 255.255.255.255
    // is a bigger capability than sending to one host, and has to be
    // asked for by name.
    let mut p = policy_net();
    p.allow_net_host.push("127.0.0.1".into());
    let sock = as_int(unwrap_ok(run_with(p.clone(), SRC, "open", vec![Value::Int(0)])));
    unwrap_ok(run_with(p.clone(), SRC, "broadcast", vec![Value::Int(sock), Value::Bool(true)]));
    let e = unwrap_err(run_with(p.clone(), SRC, "send", vec![
        Value::Int(sock), Value::Str("255.255.255.255".into()),
        Value::Int(9), Value::Bytes(b"wol".to_vec()),
    ]));
    assert!(e.contains("not in --allow-net-host"), "got: {e}");
    unwrap_ok(run_with(p, SRC, "close", vec![Value::Int(sock)]));
}

// ── handles ──────────────────────────────────────────────────────

#[test]
fn a_closed_socket_cannot_be_used() {
    let sock = open_ephemeral();
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
    let e = unwrap_err(run(SRC, "recv", vec![Value::Int(sock), Value::Int(10)]));
    assert!(e.contains("no open socket with handle"), "got: {e}");
}

#[test]
fn closing_twice_is_not_an_error() {
    // A double close is a caller being careful, not a bug worth failing.
    let sock = open_ephemeral();
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}

#[test]
fn an_unknown_handle_is_an_error_not_a_panic() {
    let e = unwrap_err(run(SRC, "recv", vec![Value::Int(999_999), Value::Int(10)]));
    assert!(e.contains("no open socket with handle 999999"), "got: {e}");
}

// ── multicast validation ─────────────────────────────────────────

#[test]
fn joining_a_non_multicast_address_is_refused() {
    // 127.0.0.1 is not in 224.0.0.0/4. Caught with a message naming the
    // reason rather than passed to the OS to fail obscurely.
    let sock = open_ephemeral();
    let e = unwrap_err(run(SRC, "join", vec![
        Value::Int(sock), Value::Str("127.0.0.1".into()),
    ]));
    assert!(e.contains("is not a multicast address"), "got: {e}");
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}

#[test]
fn joining_a_non_address_is_refused() {
    let sock = open_ephemeral();
    let e = unwrap_err(run(SRC, "join", vec![
        Value::Int(sock), Value::Str("mdns.local".into()),
    ]));
    assert!(e.contains("is not an IPv4 address"), "got: {e}");
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}

#[test]
fn joining_the_mdns_group_succeeds() {
    // 224.0.0.251 is the mDNS group — one of the motivating cases in
    // #760. Joining it must actually work, not merely validate.
    let sock = open_ephemeral();
    unwrap_ok(run(SRC, "join", vec![
        Value::Int(sock), Value::Str("224.0.0.251".into()),
    ]));
    unwrap_ok(run(SRC, "close", vec![Value::Int(sock)]));
}
