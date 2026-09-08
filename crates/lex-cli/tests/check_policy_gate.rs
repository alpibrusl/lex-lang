//! `lex check` under an effect policy: does this program's declared
//! effect footprint fit inside the policy it will be run under?
//!
//! The motivating failure is deployment drift. A service's Dockerfile
//! carries `--allow-effects io,net,sql,…`; someone adds a function that
//! declares one more effect; nothing anywhere compares the two. The
//! image builds, ships, and dies at load time on the first request —
//! the runtime *does* catch it, just far too late to be useful. Giving
//! `check` the same flags makes that a failed build instead.
//!
//! The tests that matter here are the compatibility one (a bare
//! `lex check` must behave exactly as it always has) and
//! `check_and_run_agree_*` (the gate is worthless the moment it
//! disagrees with the runtime it claims to predict).

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

/// (exit code, stdout, stderr) — violations are written to stderr.
fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn write_to_tempfile(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lex-check-policy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

/// Declares `io` and `net`; a policy granting only `io` is the drift.
const IO_AND_NET: &str = r#"import "std.io" as io
import "std.net" as net

fn fetch(url :: Str) -> [net] Result[Str, Str] { net.get(url) }

fn main() -> [io, net] Nil {
  let r := fetch("http://example.com")
  io.print("done")
}
"#;

#[test]
fn a_stale_allowlist_fails_the_check() {
    let path = write_to_tempfile("drift.lex", IO_AND_NET);
    let (code, _out, err) = run(&["check", "--allow-effects", "io", path.to_str().unwrap()]);
    assert_eq!(code, 3, "stderr: {err}");
    assert!(
        err.contains("`net` not in --allow-effects"),
        "should name the effect that is missing: {err}"
    );
    assert!(
        err.contains("at `fetch`"),
        "should name where to go fix it: {err}"
    );
}

#[test]
fn granting_the_effect_passes() {
    let path = write_to_tempfile("granted.lex", IO_AND_NET);
    let (code, out, err) = run(&[
        "check",
        "--allow-effects",
        "io,net",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stdout: {out} stderr: {err}");
    assert!(out.contains("ok"), "stdout: {out}");
}

/// The compatibility guarantee. Every `lex check` already in a
/// Makefile, a CI job or a Dockerfile passes no policy flags, and an
/// effectful program must keep passing for them. If the policy check
/// ran by default it would be `Policy::pure()` — i.e. it would reject
/// every program that does anything at all.
#[test]
fn without_policy_flags_nothing_changes() {
    let path = write_to_tempfile("unflagged.lex", IO_AND_NET);
    let (code, out, err) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("required effects: io, net"), "stdout: {out}");
}

/// ...but an *explicitly* empty allowlist is a real request — "this
/// program must stay pure" — and must be honoured. It is
/// indistinguishable from the default by value, which is why the code
/// keys off the flag's presence rather than the policy's contents.
#[test]
fn an_explicitly_empty_allowlist_still_checks() {
    let path = write_to_tempfile("must_be_pure.lex", IO_AND_NET);
    let (code, _out, err) = run(&["check", "--allow-effects", "", path.to_str().unwrap()]);
    assert_eq!(
        code, 3,
        "`--allow-effects \"\"` asks for a pure program and must be enforced: {err}"
    );
}

#[test]
fn a_pure_program_satisfies_an_empty_allowlist() {
    let path = write_to_tempfile("pure.lex", "fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    let (code, out, err) = run(&["check", "--allow-effects", "", path.to_str().unwrap()]);
    assert_eq!(code, 0, "stdout: {out} stderr: {err}");
}

/// Scoped paths are part of the policy too, and are decidable from the
/// declaration when the path is a literal.
#[test]
fn an_out_of_scope_fs_path_fails_the_check() {
    let path = write_to_tempfile(
        "scoped.lex",
        "fn save(s :: Str) -> [fs_write(\"/etc/passwd\")] Nil { save(s) }\n",
    );
    let (code, _out, err) = run(&[
        "check",
        "--allow-effects",
        "fs_write",
        "--allow-fs-write",
        "/tmp",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 3, "stderr: {err}");
    assert!(err.contains("/etc/passwd"), "stderr: {err}");
}

/// The invariant the whole feature rests on: `check` passes exactly
/// when `run` would not refuse on policy. A gate that is *stricter*
/// than the runtime fails builds that would have worked; one that is
/// *looser* lets the drift through and the crash happens in
/// production anyway. Either way the gate has stopped being worth
/// running, so assert the agreement directly rather than trusting that
/// two call sites of the same function stay wired up.
#[test]
fn check_and_run_agree_on_a_narrow_policy() {
    let path = write_to_tempfile("agree_narrow.lex", IO_AND_NET);
    let p = path.to_str().unwrap();
    let (check_code, _, _) = run(&["check", "--allow-effects", "io", p]);
    let (run_code, _, _) = run(&["run", "--allow-effects", "io", p, "main"]);
    assert_eq!(check_code, 3);
    assert_eq!(
        run_code, check_code,
        "`run` must refuse exactly what `check` refuses"
    );
}

#[test]
fn check_and_run_agree_on_a_sufficient_policy() {
    // `io.print` only; no network call, so the run reaches its own
    // exit rather than a policy refusal. What matters is that neither
    // command reports a *policy* failure (exit 3).
    let path = write_to_tempfile(
        "agree_ok.lex",
        "import \"std.io\" as io\nfn main() -> [io] Nil { io.print(\"hi\") }\n",
    );
    let p = path.to_str().unwrap();
    let (check_code, _, _) = run(&["check", "--allow-effects", "io", p]);
    let (run_code, out, err) = run(&["run", "--allow-effects", "io", p, "main"]);
    assert_eq!(check_code, 0);
    assert_ne!(
        run_code, 3,
        "check passed, so run must not refuse on policy: {out} {err}"
    );
}
