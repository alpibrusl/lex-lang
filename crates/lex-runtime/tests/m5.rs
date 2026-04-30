//! M5 acceptance per spec §7.6 and §12.5.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, CapturedSink, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn compile(src: &str) -> lex_bytecode::Program {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    lex_bytecode::compile_program(&stages)
}

fn allow(effects: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p
}

const ECHO: &str = include_str!("../../../examples/c_echo.lex");

#[test]
fn echo_runs_with_io_allowed() {
    let prog = compile(ECHO);
    let policy = allow(&["io"]);
    check_program(&prog, &policy).expect("policy must accept the program");

    let sink = Box::new(CapturedSink::default());
    let handler = DefaultHandler::new(policy).with_sink(sink);
    let mut vm = Vm::with_handler(&prog, Box::new(handler));
    let r = vm.call("echo", vec![Value::Str("hi".into())]).unwrap();
    assert_eq!(r, Value::Unit);
    // We can't read back the captured lines after Box ownership transfer;
    // this test asserts the *program ran*. The next test verifies output.
}

#[test]
fn echo_output_is_captured() {
    let prog = compile(ECHO);
    let policy = allow(&["io"]);
    check_program(&prog, &policy).expect("policy must accept the program");

    // We need shared access to the sink after the run. Use a Vec wrapped in
    // an Arc<Mutex<>>.
    use std::sync::{Arc, Mutex};
    struct SharedSink(Arc<Mutex<Vec<String>>>);
    impl lex_runtime::IoSink for SharedSink {
        fn print_line(&mut self, s: &str) { self.0.lock().unwrap().push(s.into()); }
    }
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Box::new(SharedSink(Arc::clone(&captured)));
    let handler = DefaultHandler::new(policy).with_sink(sink);
    let mut vm = Vm::with_handler(&prog, Box::new(handler));
    vm.call("echo", vec![Value::Str("hello world".into())]).unwrap();
    let lines = captured.lock().unwrap();
    assert_eq!(lines.as_slice(), &["hello world".to_string()]);
}

#[test]
fn echo_rejected_when_io_not_allowed() {
    // §7.6 acceptance criterion 1 (the disallowed half): policy check
    // refuses to run a program that declares `[io]` if `io` is not in
    // --allow-effects.
    let prog = compile(ECHO);
    let policy = Policy::pure();
    let err = check_program(&prog, &policy).expect_err("policy must reject");
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].kind, "effect_not_allowed");
    assert_eq!(err[0].effect.as_deref(), Some("io"));
    assert_eq!(err[0].at.as_deref(), Some("echo"));
}

#[test]
fn budget_is_aggregated_and_enforced() {
    // Two functions each declaring budget(50). Total 100. Runs under 200,
    // refused under 50.
    let src = r#"
fn step_a() -> [budget(50)] Int { 1 }
fn step_b() -> [budget(50)] Int { 2 }
fn run() -> [budget(50)] Int { step_a() }
"#;
    let prog = compile(src);
    let mut p = allow(&["budget"]);
    p.budget = Some(200);
    check_program(&prog, &p).expect("100 ≤ 200");

    p.budget = Some(50);
    let err = check_program(&prog, &p).expect_err("100 > 50");
    assert!(err.iter().any(|v| v.kind == "budget_exceeded"),
        "expected budget_exceeded, got {err:#?}");
}

#[test]
fn net_call_blocked_at_policy_time() {
    // §12.5: a program declaring `[net]` without --allow-effects net is
    // rejected before any execution starts.
    let src = "fn fetch() -> [net] Int { 0 }\n";
    let prog = compile(src);
    let policy = allow(&["io"]); // net not allowed
    let err = check_program(&prog, &policy).expect_err("must reject");
    assert!(err.iter().any(|v| v.effect.as_deref() == Some("net")
        && v.kind == "effect_not_allowed"),
        "expected net violation, got {err:#?}");
}

#[test]
fn net_call_allowed_when_in_allowlist() {
    let src = "fn fetch() -> [net] Int { 0 }\n";
    let prog = compile(src);
    let policy = allow(&["net"]);
    check_program(&prog, &policy).expect("net allowed");
}

#[test]
fn fs_read_path_must_be_under_allowlist() {
    let src = r#"fn read_data() -> [fs_read("/etc")] Int { 0 }
"#;
    let prog = compile(src);
    let mut policy = allow(&["fs_read"]);
    policy.allow_fs_read = vec!["/var".into()];
    let err = check_program(&prog, &policy).expect_err("/etc not under /var");
    assert!(err.iter().any(|v| v.kind == "fs_path_not_allowed"
        && v.path.as_deref() == Some("/etc")));

    policy.allow_fs_read = vec!["/etc".into()];
    check_program(&prog, &policy).expect("now /etc is allowed");
}
