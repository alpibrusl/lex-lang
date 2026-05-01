//! End-to-end tests for `lex tool-registry serve`.
//!
//! Spawns the binary, registers tools via HTTP, invokes them, and
//! asserts on the manifest + runtime behavior. Each test uses its
//! own port so they can run in parallel.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

struct Server {
    child: Child,
    port: u16,
}
impl Drop for Server {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

#[allow(clippy::zombie_processes)] // Drop handler kills + waits.
fn start_server(port: u16) -> Server {
    let child = Command::new(lex_bin())
        .args(["tool-registry", "serve", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");
    // Wait for the bind. Polling is more reliable than a sleep.
    for _ in 0..40 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Server { child, port };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server didn't bind on {port}");
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

#[test]
fn register_pure_tool_then_invoke_returns_output() {
    let s = start_server(18601);
    let (status, body) = http(s.port, "POST", "/tools",
        r#"{"name":"greeter","body":"str.concat(\"hi, \", input)","allowed_effects":[]}"#);
    assert_eq!(status, 201, "register: {body}");
    assert!(body.contains("\"id\""), "body: {body}");
    assert!(body.contains("\"endpoint\""), "body: {body}");

    let (status, body) = http(s.port, "POST", "/tools/t00000001/invoke",
        r#"{"input":"world"}"#);
    assert_eq!(status, 200, "invoke: {body}");
    assert!(body.contains("\"output\":\"hi, world\""), "body: {body}");
}

#[test]
fn malicious_tool_rejected_at_registration_with_type_check_error() {
    let s = start_server(18602);
    // Body uses io.read but allowed_effects is [net]. Type checker
    // rejects at register-time; tool never gets stored.
    let (status, body) = http(s.port, "POST", "/tools",
        r#"{"name":"bad","body":"match io.read(\"/etc/passwd\") { Ok(s) => s, Err(e) => e }","allowed_effects":["net"]}"#);
    assert_eq!(status, 400, "expected 400; body: {body}");
    assert!(body.contains("type_check"), "body: {body}");
    assert!(body.contains("effect `io`"), "body: {body}");

    // Confirm nothing was stored.
    let (status, body) = http(s.port, "GET", "/tools", "");
    assert_eq!(status, 200);
    assert_eq!(body, "[]", "tools list should be empty: {body}");
}

#[test]
fn manifest_endpoint_returns_tool_record() {
    let s = start_server(18603);
    let (status, _) = http(s.port, "POST", "/tools",
        r#"{"name":"echo","body":"input","allowed_effects":[]}"#);
    assert_eq!(status, 201);

    let (status, body) = http(s.port, "GET", "/tools/t00000001", "");
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"name\":\"echo\""), "body: {body}");
    assert!(body.contains("\"allowed_effects\":[]"), "body: {body}");
}

#[test]
fn unknown_tool_id_returns_404() {
    let s = start_server(18604);
    let (status, body) = http(s.port, "GET", "/tools/t99999999", "");
    assert_eq!(status, 404, "body: {body}");
    let (status, _) = http(s.port, "POST", "/tools/t99999999/invoke", r#"{"input":"x"}"#);
    assert_eq!(status, 404);
}

#[test]
fn unknown_path_returns_404() {
    let s = start_server(18605);
    let (status, _) = http(s.port, "GET", "/no/such/path", "");
    assert_eq!(status, 404);
}

#[test]
fn list_tools_after_two_registrations() {
    let s = start_server(18606);
    // Two tools, both pure. We pre-build the JSON via an escaped
    // body string — otherwise inner `"` from str.concat literals
    // collide with JSON delimiters.
    let cases: &[(&str, &str)] = &[
        ("a", "input"),
        ("b", r#"str.concat(\"x\", input)"#),
    ];
    for (name, body_lex) in cases {
        let body = format!(r#"{{"name":"{name}","body":"{body_lex}","allowed_effects":[]}}"#);
        let (status, resp) = http(s.port, "POST", "/tools", &body);
        assert_eq!(status, 201, "register {name}: {resp}");
    }
    let (status, body) = http(s.port, "GET", "/tools", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"name\":\"a\""), "body: {body}");
    assert!(body.contains("\"name\":\"b\""), "body: {body}");
}
