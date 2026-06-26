//! `std.vcs` — content-addressed blob store (#5). Exercises the builtin
//! dispatch end-to-end through the VM: put_blob → get_blob round-trips, and
//! ref_set → ref_get binds a name to a sha. The store root is a per-test
//! tempdir via $LEX_STORE_ROOT, so this writes nothing to the real store.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

const SRC: &str = r#"
import "std.vcs" as vcs

# Put content, then read it back by the returned sha. Returns the content.
fn put_get(content :: Str) -> [vcs, fs_write, fs_read] Str {
  match vcs.put_blob(content) {
    Err(e) => str_err(e),
    Ok(sha) => match vcs.get_blob(sha) {
      Err(e) => str_err(e),
      Ok(c)  => c,
    },
  }
}

# Bind ns/key to sha, then resolve it back. Returns the resolved sha.
fn ref_roundtrip(ns :: Str, key :: Str, sha :: Str) -> [vcs, fs_write, fs_read] Str {
  match vcs.ref_set(ns, key, sha) {
    Err(e) => str_err(e),
    Ok(_)  => match vcs.ref_get(ns, key) {
      Err(e) => str_err(e),
      Ok(s)  => s,
    },
  }
}

fn str_err(e :: Str) -> Str { e }
"#;

fn policy_with(effects: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p
}

fn compile() -> Arc<lex_bytecode::Program> {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    Arc::new(compile_program(&stages))
}

fn call_str(bc: &Arc<lex_bytecode::Program>, func: &str, args: Vec<Value>) -> String {
    let handler = DefaultHandler::new(policy_with(&["vcs", "fs_write", "fs_read"]))
        .with_program(Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    match vm.call(func, args).expect("vm call") {
        Value::Str(s) => s.to_string(),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn blob_and_ref_roundtrip_through_vm() {
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LEX_STORE_ROOT", tmp.path());

    let bc = compile();

    // put_blob then get_blob returns the original content.
    let got = call_str(&bc, "put_get", vec![Value::Str("hello loom artifact".into())]);
    assert_eq!(got, "hello loom artifact");

    // ref_set/ref_get binds a branch-per-sprint name to a sha and resolves it.
    let sha = "deadbeefcafe";
    let resolved = call_str(
        &bc,
        "ref_roundtrip",
        vec![
            Value::Str("loom/sprint-demo".into()),
            Value::Str("build-node".into()),
            Value::Str(sha.into()),
        ],
    );
    assert_eq!(resolved, sha);

    // Layout matches lex-store's blob CAS.
    assert!(tmp.path().join("blobrefs").join("loom").join("sprint-demo").join("build-node").exists());

    std::env::remove_var("LEX_STORE_ROOT");
}
