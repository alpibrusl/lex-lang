//! Integration tests for examples/inbox_app.lex — the typed-handler
//! event router. Verifies that classification dispatches to the right
//! handler and that each handler honors its declared effect scope.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn spawn_inbox_server(port: u16) {
    // Per-port tmp paths so parallel tests don't collide on shared
    // log files. Each test owns its own port → owns its own logs.
    let src = include_str!("../../../examples/inbox_app.lex")
        .replace("net.serve(8200,", &format!("net.serve({port},"))
        .replace("/tmp/lex_inbox_spam.log",
                 &format!("/tmp/lex_inbox_spam_{port}.log"))
        .replace("/tmp/lex_inbox_followups.log",
                 &format!("/tmp/lex_inbox_followups_{port}.log"));
    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["io".into(), "net".into(), "time".into()]
        .into_iter().collect::<BTreeSet<_>>();
    policy.allow_fs_write = vec![PathBuf::from("/tmp")];
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(200));
}

fn post(port: u16, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

fn cleanup(port: u16) {
    let _ = std::fs::remove_file(format!("/tmp/lex_inbox_spam_{port}.log"));
    let _ = std::fs::remove_file(format!("/tmp/lex_inbox_followups_{port}.log"));
}

#[test]
fn spam_subject_routes_to_spam_handler_and_writes_log() {
    let port = 18401;
    cleanup(port);
    spawn_inbox_server(port);
    let (status, body) = post(port, "/hook",
        r#"{"from":"a@b","subject":"win a prize today","body":"x"}"#);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"logged\""), "body: {body}");
    let log = std::fs::read_to_string(format!("/tmp/lex_inbox_spam_{port}.log"))
        .expect("spam log written");
    assert!(log.contains("a@b"), "log content: {log}");
    assert!(log.contains("win a prize"), "log content: {log}");
}

#[test]
fn followup_subject_writes_followup_log_with_timestamp() {
    let port = 18402;
    cleanup(port);
    spawn_inbox_server(port);
    let (status, body) = post(port, "/hook",
        r#"{"from":"a@b","subject":"please follow up next week","body":"x"}"#);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("scheduled at"), "body: {body}");
    let log = std::fs::read_to_string(format!("/tmp/lex_inbox_followups_{port}.log"))
        .expect("followup log written");
    assert!(log.contains("follow-up"), "log content: {log}");
    // timestamp is a unix-epoch integer, so the line starts with digits.
    let first = log.chars().next().unwrap_or(' ');
    assert!(first.is_ascii_digit(), "expected leading timestamp; got: {log}");
}

#[test]
fn other_subject_returns_pure_ignored_response() {
    let port = 18403;
    cleanup(port);
    spawn_inbox_server(port);
    let (status, body) = post(port, "/hook",
        r#"{"from":"a@b","subject":"weekly newsletter","body":"x"}"#);
    assert_eq!(status, 200);
    assert!(body.contains("ignored"), "body: {body}");
    // No side effects on disk *for this port's log files*.
    assert!(!PathBuf::from(format!("/tmp/lex_inbox_spam_{port}.log")).exists());
    assert!(!PathBuf::from(format!("/tmp/lex_inbox_followups_{port}.log")).exists());
}

#[test]
fn unknown_path_returns_404() {
    let port = 18404;
    spawn_inbox_server(port);
    let (status, _body) = post(port, "/nope", "{}");
    assert_eq!(status, 404);
}
