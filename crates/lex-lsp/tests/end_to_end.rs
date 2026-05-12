//! End-to-end smoke test for `lex-lsp` phase 1 (#304).
//!
//! Spawns the compiled binary, drives it through `initialize` →
//! `didOpen` → expects `publishDiagnostics`, asserting the JSON
//! envelope the editor would receive. This is the minimum proof
//! that the LSP loop actually works over stdio, beyond the unit
//! tests that exercise `diagnostics_for_source` directly.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

fn lsp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex-lsp")
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn spawn() -> Self {
        let mut child = Command::new(lsp_bin())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lex-lsp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Self { child, stdin, reader: BufReader::new(stdout) }
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        // LSP framing: Content-Length + CRLF + CRLF, then body.
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read one LSP frame and parse it as JSON.
    fn recv(&mut self) -> Value {
        let mut content_length: Option<usize> = None;
        loop {
            let mut header = String::new();
            self.reader.read_line(&mut header).expect("read header");
            if header == "\r\n" || header.is_empty() {
                break;
            }
            if let Some(rest) = header.strip_prefix("Content-Length:") {
                content_length = Some(rest.trim().parse().unwrap());
            }
        }
        let n = content_length.expect("Content-Length");
        let mut body = vec![0u8; n];
        self.reader.read_exact(&mut body).expect("read body");
        serde_json::from_slice(&body).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn type_error_round_trips_through_lsp_protocol() {
    let mut s = Server::spawn();

    // 1. Initialize.
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "capabilities": {},
            "processId": null,
            "rootUri": null,
        }
    }));
    let init_resp = s.recv();
    assert_eq!(init_resp["id"], 1, "initialize response correlation");
    assert!(
        init_resp["result"]["capabilities"]["textDocumentSync"].is_number()
            || init_resp["result"]["capabilities"]["textDocumentSync"].is_object(),
        "server declares textDocumentSync: {init_resp}"
    );

    // 2. Initialized notification (no response expected).
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));

    // 3. didOpen a Lex doc with a type error.
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_test.lex",
                "languageId": "lex",
                "version": 1,
                "text": "fn bad(x :: Int) -> Int { \"oops\" }\n"
            }
        }
    }));

    // 4. Expect a publishDiagnostics notification.
    let diag = s.recv();
    assert_eq!(diag["method"], "textDocument/publishDiagnostics");
    assert_eq!(diag["params"]["uri"], "file:///tmp/lsp_test.lex");
    let diags = diag["params"]["diagnostics"].as_array().expect("array");
    assert_eq!(diags.len(), 1, "exactly one diagnostic: {diag}");
    let d = &diags[0];
    assert_eq!(d["severity"], 1, "ERROR severity");
    assert_eq!(d["source"], "lex");
    assert_eq!(d["code"], "type-mismatch", "rule_tag as code");
    assert_eq!(d["data"]["rule_tag"], "type-mismatch");
    assert!(d["data"]["rule_explanation"].as_str().is_some());
    assert!(d["data"]["suggested_transform"].is_object());

    // 5. Shutdown + exit.
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "shutdown",
        "params": null
    }));
    let _ = s.recv(); // shutdown response
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    }));

    // Give the child a moment to exit cleanly.
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
fn hover_returns_signature_markdown() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let _ = s.recv();
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    let text = "fn echo(msg :: Str) -> [io, budget(5)] Nil { msg }\n";
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_hover.lex",
                "languageId": "lex",
                "version": 1,
                "text": text,
            }
        }
    }));
    let _ = s.recv(); // publishDiagnostics

    // Cursor on `echo` (line 0, character 4).
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": "file:///tmp/lsp_hover.lex" },
            "position": { "line": 0, "character": 4 }
        }
    }));
    let resp = s.recv();
    assert_eq!(resp["id"], 2);
    let contents = &resp["result"]["contents"];
    assert_eq!(contents["kind"], "markdown");
    let value = contents["value"].as_str().expect("hover value");
    assert!(value.contains("fn echo"), "sig in hover: {value}");
    assert!(value.contains("io"), "effects in hover: {value}");
    assert!(value.contains("budget"), "budget in hover: {value}");
}

#[test]
fn definition_jumps_to_fn_declaration() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let _ = s.recv();
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    let text = "\
fn double(n :: Int) -> Int { n + n }
fn caller() -> Int { double(2) }
";
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_def.lex",
                "languageId": "lex",
                "version": 1,
                "text": text,
            }
        }
    }));
    let _ = s.recv(); // publishDiagnostics

    // Cursor on the `double` call site in line 1.
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/definition",
        "params": {
            "textDocument": { "uri": "file:///tmp/lsp_def.lex" },
            "position": { "line": 1, "character": 23 }
        }
    }));
    let resp = s.recv();
    let result = &resp["result"];
    let uri = result["uri"].as_str().expect("uri");
    assert_eq!(uri, "file:///tmp/lsp_def.lex");
    let line = result["range"]["start"]["line"].as_u64().expect("line");
    assert_eq!(line, 0, "double is defined on line 0");
}

#[test]
fn completion_lists_fns_and_imports() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let _ = s.recv();
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    let text = "\
import \"std.io\" as io
fn helper() -> Int { 1 }
fn other() -> Int { 2 }
";
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_compl.lex",
                "languageId": "lex",
                "version": 1,
                "text": text,
            }
        }
    }));
    let _ = s.recv(); // publishDiagnostics

    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/completion",
        "params": {
            "textDocument": { "uri": "file:///tmp/lsp_compl.lex" },
            "position": { "line": 3, "character": 0 }
        }
    }));
    let resp = s.recv();
    let arr = resp["result"].as_array().expect("array");
    let labels: Vec<&str> = arr.iter().filter_map(|i| i["label"].as_str()).collect();
    assert!(labels.contains(&"helper"));
    assert!(labels.contains(&"other"));
    assert!(labels.contains(&"io"));
}

#[test]
fn code_action_surfaces_suggested_transform() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let init = s.recv();
    // Capability is advertised so editors enable the lightbulb.
    assert!(
        init["result"]["capabilities"]["codeActionProvider"] == json!(true)
            || init["result"]["capabilities"]["codeActionProvider"].is_object(),
        "codeActionProvider must be advertised: {init}"
    );
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    let text = "fn bad(x :: Int) -> Int { \"oops\" }\n";
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_qf.lex",
                "languageId": "lex",
                "version": 1,
                "text": text,
            }
        }
    }));
    // Pull the publishDiagnostics so we can echo its diagnostic
    // back into the codeAction request — that's the realistic
    // editor-side flow.
    let diag = s.recv();
    let diagnostics = diag["params"]["diagnostics"].clone();
    assert!(diagnostics.as_array().is_some_and(|a| !a.is_empty()));

    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/codeAction",
        "params": {
            "textDocument": { "uri": "file:///tmp/lsp_qf.lex" },
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
            "context": { "diagnostics": diagnostics }
        }
    }));
    let resp = s.recv();
    let actions = resp["result"].as_array().expect("array of actions");
    assert_eq!(actions.len(), 1, "one action for the type-mismatch: {resp}");
    let a = &actions[0];
    let title = a["title"].as_str().unwrap_or("");
    assert!(title.starts_with("Lex:"), "title prefixed: {title}");
    assert!(title.contains("ReplaceMatchArm"), "kind_hint in title: {title}");
    assert_eq!(a["kind"], "quickfix");
    assert_eq!(a["isPreferred"], true);
    // The suggestion data round-trips so client extensions can
    // pipe it to `lex repair --apply --transform '<json>'`.
    let data = &a["data"];
    assert_eq!(data["rule_tag"], "type-mismatch");
    assert!(data["details"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn code_action_returns_empty_when_no_diagnostics() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let _ = s.recv();
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_no_qf.lex",
                "languageId": "lex",
                "version": 1,
                "text": "fn ok_fn(x :: Int) -> Int { x + 1 }\n",
            }
        }
    }));
    let _ = s.recv(); // empty diagnostics

    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "textDocument/codeAction",
        "params": {
            "textDocument": { "uri": "file:///tmp/lsp_no_qf.lex" },
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
            "context": { "diagnostics": [] }
        }
    }));
    let resp = s.recv();
    let actions = resp["result"].as_array().expect("array");
    assert!(actions.is_empty(), "no diagnostics → no actions: {resp}");
}

#[test]
fn clean_program_emits_empty_diagnostics() {
    let mut s = Server::spawn();
    s.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {}, "processId": null, "rootUri": null }
    }));
    let _ = s.recv();
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    s.send(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": "file:///tmp/lsp_clean.lex",
                "languageId": "lex",
                "version": 1,
                "text": "fn add(x :: Int, y :: Int) -> Int { x + y }\n"
            }
        }
    }));
    let diag = s.recv();
    assert_eq!(diag["method"], "textDocument/publishDiagnostics");
    let arr = diag["params"]["diagnostics"].as_array().unwrap();
    assert!(arr.is_empty(), "clean program: no diagnostics, got {diag}");
}
