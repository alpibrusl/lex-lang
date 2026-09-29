//! `std.panic.todo` — a stub marker that type-checks against any
//! signature (its return type is `Never`, which unifies with whatever
//! the caller expects) and, called for real, aborts immediately with
//! its message and no effect grant of its own.

use std::io::Write;
use std::process::Command;

fn lex(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lex")).args(args).output().expect("run lex")
}

fn write_src(dir: &std::path::Path, name: &str, src: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(src.as_bytes()).unwrap();
    path
}

#[test]
fn unifies_against_bool_str_list_and_an_effectful_signature() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_src(
        tmp.path(),
        "m.lex",
        r#"
import "std.panic" as panic

fn a(s :: Str) -> Str {
  panic.todo("a")
}

fn b(n :: Int) -> Bool {
  panic.todo("b")
}

fn c(s :: Str, w :: Int) -> List[Str] {
  panic.todo("c")
}

fn d() -> [net] Nil {
  panic.todo("d")
}
"#,
    );
    let out = lex(&["check", path.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn aborts_immediately_with_its_message_and_no_effect_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_src(
        tmp.path(),
        "m.lex",
        r#"
import "std.panic" as panic

fn slugify(s :: Str) -> Str {
  panic.todo("slugify")
}
"#,
    );
    // No --allow-effects at all: a pure signature stub needs none.
    let out = lex(&["run", "--allow-effects", "", path.to_str().unwrap(), "slugify", "\"hi\""]);
    assert!(!out.status.success(), "an unimplemented stub must not silently succeed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not yet implemented: slugify"), "stderr: {stderr}");
    assert!(!stderr.contains("step limit"), "must fail immediately, not via the step budget: {stderr}");
}

#[test]
fn an_effectful_stub_still_only_needs_its_own_declared_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_src(
        tmp.path(),
        "m.lex",
        r#"
import "std.panic" as panic

fn serve() -> [net] Nil {
  panic.todo("serve")
}
"#,
    );
    let out = lex(&["run", "--allow-effects", "net", path.to_str().unwrap(), "serve"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not yet implemented: serve"), "stderr: {stderr}");
    assert!(!stderr.contains("not in --allow-effects") && !stderr.contains("effect_not_allowed"), "todo() must not itself require a `panic` effect: {stderr}");
}
