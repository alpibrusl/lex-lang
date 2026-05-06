//! `agent.call_mcp` end-to-end against a real `lex serve --mcp`
//! subprocess (#185). The client spawns the server, sends the
//! JSON-RPC handshake, forwards `tools/call`, and returns the
//! result. The test exercises the full round-trip.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

/// Path to the `lex` binary built for this test run. Provided
/// by Cargo when integration tests are compiled. The runtime
/// uses this binary (with `serve --mcp`) as the MCP server
/// fixture.
fn lex_bin_path() -> String {
    // `lex-runtime`'s test crate doesn't depend on the `lex`
    // binary directly, so `CARGO_BIN_EXE_lex` isn't set. Build
    // the path from `CARGO_TARGET_TMPDIR` / `CARGO_MANIFEST_DIR`
    // by climbing to the workspace target dir. Standard layout:
    // `<workspace>/target/debug/lex` (or release/).
    let manifest = env!("CARGO_MANIFEST_DIR");
    let workspace = std::path::Path::new(manifest)
        .parent().unwrap()  // crates/
        .parent().unwrap(); // workspace root
    // Honor CARGO_TARGET_DIR if set (CI uses it).
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| workspace.join("target"));
    // Profile is the leaf component of the test executable's
    // path. `std::env::current_exe()` gives `.../target/<profile>/deps/<test_bin>-...`.
    let me = std::env::current_exe().unwrap();
    let profile = me
        .ancestors()
        .find_map(|p| {
            let name = p.file_name()?.to_str()?;
            if name == "deps" {
                p.parent()
                    .and_then(|q| q.file_name())
                    .and_then(|s| s.to_str())
                    .map(String::from)
            } else { None }
        })
        .unwrap_or_else(|| "debug".into());
    target_dir.join(profile).join("lex").to_string_lossy().into_owned()
}

fn run_program(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
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

#[test]
fn call_mcp_round_trips_lex_check_through_lex_serve_mcp() {
    // Use the lex binary's own `serve --mcp` as the MCP server
    // fixture. It exposes `lex_check` as one of its tools.
    // Skip the test if the binary isn't where we expect — local
    // dev sometimes builds with non-standard layouts.
    let lex_bin = lex_bin_path();
    if !std::path::Path::new(&lex_bin).exists() {
        eprintln!("skipping: lex binary not at {lex_bin}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let server_cmd = format!(
        "{lex_bin} serve --mcp --store {}",
        tmp.path().display(),
    );

    // The Lex program calls call_mcp with a small Lex source as
    // the input to the `lex_check` MCP tool. The expected result
    // is `Ok({"ok":true,...})` — a clean type-check.
    let src = r#"
import "std.agent" as agent
import "std.str" as str

fn check_remotely(server :: Str, source :: Str) -> [mcp] Result[Str, Str] {
  let body := str.concat("{\"source\":\"", str.concat(source, "\"}"))
  agent.call_mcp(server, "lex_check", body)
}
"#;
    // Lex source for the MCP server to type-check. Picks a
    // trivial but non-empty fn so success is unambiguous.
    let lex_src = r#"fn id(n :: Int) -> Int { n }"#;
    let v = run_program(src, "check_remotely",
        vec![Value::Str(server_cmd), Value::Str(lex_src.to_string())],
        Policy::permissive());
    match &v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Str(s) => {
                // MCP wraps tool results in `{"content":[{"text":...}],
                // "isError": false}`. The inner `text` is the
                // `lex_check` JSON; on a clean check it contains
                // `"ok":true`. Match either escaped or unescaped
                // since whether the inner text is JSON-quoted
                // depends on the host's stringification.
                assert!(s.contains("\\\"ok\\\":true") || s.contains("\"ok\":true"),
                    "expected ok=true from lex_check, got: {s}");
                assert!(s.contains("\"isError\":false"),
                    "expected isError=false, got: {s}");
            }
            other => panic!("expected Str, got {other:?}"),
        },
        Value::Variant { name, args } if name == "Err" => {
            panic!("call_mcp returned Err: {:?}", args[0]);
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn call_mcp_with_invalid_args_json_returns_err() {
    let src = r#"
import "std.agent" as agent

fn bad_args(server :: Str) -> [mcp] Result[Str, Str] {
  agent.call_mcp(server, "lex_check", "this is not json")
}
"#;
    let v = run_program(src, "bad_args",
        vec![Value::Str("nonexistent_command".into())],
        Policy::permissive());
    match &v {
        Value::Variant { name, args } if name == "Err" => match &args[0] {
            Value::Str(s) => assert!(s.contains("not valid JSON"),
                "expected JSON-parse error, got: {s}"),
            other => panic!("expected Str inside Err, got {other:?}"),
        },
        other => panic!("expected Err, got {other:?}"),
    }
}
