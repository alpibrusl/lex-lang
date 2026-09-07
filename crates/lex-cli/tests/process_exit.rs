//! `std.process.exit` — a Lex program can set its caller's exit status
//! (#754).
//!
//! These drive the real binary rather than the VM, because the thing
//! under test *is* the process's exit status. A unit test on
//! `VmError::ProcessExit` would pass while `lex run` still returned 0,
//! which is precisely the bug being fixed.
//!
//! The shape follows the issue's option 3: the ability to set the
//! caller's exit status is an effect on the outside world, so it lives
//! in the effect row — `[proc_exit]` — and is refused like any other
//! capability when not granted.

use std::io::Write;
use std::process::Command;

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

/// Write a .lex file into a temp dir and return its path.
fn write_lex(dir: &tempfile::TempDir, name: &str, src: &str) -> String {
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("creating the fixture");
    f.write_all(src.as_bytes()).expect("writing the fixture");
    path.to_string_lossy().into_owned()
}

fn run(file: &str, func: &str, effects: &str) -> std::process::Output {
    Command::new(lex_bin())
        .args(["run", "--allow-effects", effects, file, func])
        .output()
        .expect("running lex")
}

const EXIT_LEX: &str = r#"
import "std.io" as io
import "std.process" as process

fn ok() -> [io, proc_exit] Int { let __0 := io.print("fine"); 0 }

fn bad() -> [io, proc_exit] Int {
    let __0 := io.print("problems");
    let __1 := process.exit(1);
    0
}

fn zero() -> [io, proc_exit] Int {
    let __0 := io.print("clean");
    let __1 := process.exit(0);
    0
}

fn big() -> [io, proc_exit] Int { let __0 := process.exit(300); 0 }

fn twice() -> [io, proc_exit] Int {
    let __0 := process.exit(7);
    let __1 := process.exit(9);
    0
}

fn after() -> [io, proc_exit] Int {
    let __0 := process.exit(4);
    let __1 := io.print("UNREACHABLE");
    0
}
"#;

/// The issue's own reproduction, both halves. Before this, `bad`
/// printed "problems" and exited 0 — a program signalling failure that
/// a shell read as success.
#[test]
fn a_program_can_set_a_non_zero_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);

    let out = run(&f, "ok", "io,proc_exit");
    assert_eq!(out.status.code(), Some(0), "a program that returns exits 0");
    assert!(String::from_utf8_lossy(&out.stdout).contains("fine"));

    let out = run(&f, "bad", "io,proc_exit");
    assert_eq!(out.status.code(), Some(1), "the 1 must not be discarded");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("problems"),
        "output written before the exit still reaches the caller"
    );
}

/// A deliberate `exit(0)` is a successful run that chose to stop, not a
/// failure, and not something the caller can distinguish from a normal
/// return. That is the point: the status is the contract.
#[test]
fn exit_zero_is_success() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);
    let out = run(&f, "zero", "io,proc_exit");
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("clean"));
}

/// **Clamped, not wrapped.** A shell sees `status & 0xff`, so a naive
/// cast would deliver `exit(300)` as 44 and `exit(256)` as **0** — a
/// program reporting failure that reads as success, which is the one
/// outcome this feature must never produce.
#[test]
fn an_out_of_range_status_clamps_rather_than_wrapping() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);
    let out = run(&f, "big", "io,proc_exit");
    assert_eq!(
        out.status.code(),
        Some(255),
        "300 clamps to 255; wrapping would have produced 44"
    );
}

/// The first exit wins, and nothing after it runs. A program that
/// reaches a second `exit` has already stopped at the first, so the
/// later call cannot restate the verdict.
#[test]
fn the_first_exit_wins_and_nothing_after_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);

    let out = run(&f, "twice", "io,proc_exit");
    assert_eq!(out.status.code(), Some(7), "not 9");

    let out = run(&f, "after", "io,proc_exit");
    assert_eq!(out.status.code(), Some(4));
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("UNREACHABLE"),
        "the exit unwinds; statements after it do not run"
    );
}

/// It is a capability like any other. Without `proc_exit` in the row's
/// grant the program is refused before it runs, with the policy exit
/// code (3) rather than the status it asked for.
#[test]
fn the_effect_is_gated_like_every_other_capability() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);
    let out = run(&f, "bad", "io");
    assert_eq!(
        out.status.code(),
        Some(3),
        "a policy refusal is not the program's chosen status"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let both = format!("{stdout}{stderr}");
    assert!(
        both.contains("proc_exit"),
        "the refusal names the missing effect: {both}"
    );
}

/// `proc` and `proc_exit` are separate authorities. A program granted
/// the right to spawn subprocesses has not thereby been granted the
/// right to decide what its invoker sees — the same split, for the same
/// reason, as `fs` → `fs_walk` / `fs_write`.
#[test]
fn spawning_a_subprocess_does_not_confer_the_right_to_exit() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);
    let out = run(&f, "bad", "io,proc");
    assert_eq!(
        out.status.code(),
        Some(3),
        "`proc` must not stand in for `proc_exit`"
    );
}

/// `--output json` still produces an envelope. A pipeline reading the
/// JSON should not have to infer from a missing document that the
/// program chose to stop.
#[test]
fn a_json_run_still_emits_an_envelope_when_the_program_exits() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_lex(&dir, "exit.lex", EXIT_LEX);
    let out = Command::new(lex_bin())
        .args([
            "--output",
            "json",
            "run",
            "--allow-effects",
            "io,proc_exit",
            &f,
            "bad",
        ])
        .output()
        .expect("running lex");
    assert_eq!(out.status.code(), Some(1));
    // The envelope is pretty-printed, so it spans lines; the program's
    // own stdout precedes it.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("no JSON envelope in: {stdout}"));
    let envelope: serde_json::Value = serde_json::from_str(&stdout[start..])
        .unwrap_or_else(|e| panic!("envelope did not parse ({e}): {stdout}"));
    assert_eq!(
        envelope["data"]["exit_code"].as_i64(),
        Some(1),
        "the envelope names the status: {envelope}"
    );
}
