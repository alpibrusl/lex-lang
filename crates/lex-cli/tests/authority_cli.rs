//! `lex authority`: the least grant a program provably needs, and the
//! delta a change makes to it.
//!
//! The cases here are the ones a reviewer relies on being true — the
//! derivation is tight, a new host is a widening, a removed one is a
//! narrowing, `--fail-on` is the CI gate — plus the two refusals that
//! keep the answer honest: nothing is derived from a program that does
//! not type-check, and an effect outside the trust lattice is reported
//! rather than quietly dropped.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

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

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lex-authority-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(dir: &std::path::Path, name: &str, src: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, src).unwrap();
    p.to_str().unwrap().to_string()
}

const APPROVED: &str = r#"
import "std.io" as io
import "std.net" as net
fn announce(r :: Str) -> [io] Unit { io.print(r) }
fn submit(r :: Str) -> [net, net("results.demo.internal")] Result[Str, Str] {
  net.get("https://results.demo.internal/submit")
}
"#;

const WIDENED: &str = r#"
import "std.io" as io
import "std.env" as env
import "std.net" as net
fn announce(r :: Str) -> [io] Unit { io.print(r) }
fn submit(r :: Str) -> [net, net("results.demo.internal")] Result[Str, Str] {
  net.get("https://results.demo.internal/submit")
}
fn token() -> [env] Option[Str] { env.get("TELEMETRY_TOKEN") }
fn push(r :: Str) -> [env, net, net("telemetry.vendor.example")] Result[Str, Str] {
  match token() {
    Some(t) => net.get("https://telemetry.vendor.example/ingest"),
    None => Err("no token")
  }
}
"#;

const NARROWED: &str = r#"
import "std.io" as io
fn announce(r :: Str) -> [io] Unit { io.print(r) }
"#;

#[test]
fn derive_reports_the_minimal_grant_and_why_it_is_tight() {
    let d = dir("derive");
    let p = write(&d, "v1.lex", APPROVED);
    let (code, out, err) = run(&["authority", "derive", &p]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("fs=none net=allowlist exec=none"), "{out}");
    assert!(out.contains("results.demo.internal"), "{out}");
    // The minimality claim is printed, not just made.
    assert!(
        out.contains("lowering to `loopback` rejects"),
        "expected the witness line, got: {out}"
    );
}

#[test]
fn derive_names_the_function_that_needs_each_effect() {
    let d = dir("contributors");
    let p = write(&d, "v1.lex", APPROVED);
    let (_, out, _) = run(&["authority", "derive", &p]);
    // "something needs net" is less useful than "submit needs net".
    let net_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("net "))
        .unwrap_or_default();
    assert!(net_line.contains("submit"), "got: {out}");
}

#[test]
fn a_pure_program_needs_no_authority_at_all() {
    let d = dir("pure");
    let p = write(&d, "p.lex", "fn add(a :: Int, b :: Int) -> Int { a + b }\n");
    let (code, out, _) = run(&["authority", "derive", &p]);
    assert_eq!(code, 0);
    assert!(out.contains("fs=none net=none exec=none"), "{out}");
    assert!(out.contains("needs no authority at all"), "{out}");
}

#[test]
fn a_new_host_is_a_widening_and_fail_on_makes_it_a_ci_gate() {
    let d = dir("widen");
    let base = write(&d, "v1.lex", APPROVED);
    let head = write(&d, "v2.lex", WIDENED);

    // Reported, but not refused, without --fail-on: `diff` stays usable
    // as a PR annotator.
    let (code, out, _) = run(&["authority", "diff", "--base", &base, "--head", &head]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("+ telemetry.vendor.example"), "{out}");
    assert!(out.contains("WIDENING"), "{out}");

    let (code, _, _) = run(&[
        "authority",
        "diff",
        "--base",
        &base,
        "--head",
        &head,
        "--fail-on",
        "widening",
    ]);
    assert_eq!(code, 8, "a widening must fail the gate with exit 8");
}

#[test]
fn an_off_lattice_effect_is_reported_though_no_grant_would_catch_it() {
    let d = dir("offlattice");
    let base = write(&d, "v1.lex", APPROVED);
    let head = write(&d, "v2.lex", WIDENED);
    let (_, out, _) = run(&["authority", "diff", "--base", &base, "--head", &head]);
    // `env` ranks on no dimension, so the coarse grant cannot move on
    // it. It still belongs in the review, with who introduced it.
    assert!(out.contains("off-lattice  + env"), "{out}");
    assert!(out.contains("push"), "expected the contributing fn: {out}");
}

#[test]
fn dropping_the_network_is_a_narrowing() {
    let d = dir("narrow");
    let base = write(&d, "v1.lex", APPROVED);
    let head = write(&d, "v3.lex", NARROWED);
    let (code, out, _) = run(&[
        "authority",
        "diff",
        "--base",
        &base,
        "--head",
        &head,
        "--fail-on",
        "widening",
    ]);
    assert_eq!(code, 0, "a narrowing is not a refusal: {out}");
    assert!(out.contains("NARROWING"), "{out}");
    assert!(out.contains("- results.demo.internal"), "{out}");

    // …but `--fail-on any` pins authority exactly, for code whose grant
    // is fixed by an external approval.
    let (code, _, _) = run(&[
        "authority",
        "diff",
        "--base",
        &base,
        "--head",
        &head,
        "--fail-on",
        "any",
    ]);
    assert_eq!(code, 8);
}

#[test]
fn the_same_source_derives_the_same_authority_id() {
    let d = dir("stable");
    let a = write(&d, "a.lex", APPROVED);
    let b = write(&d, "b.lex", APPROVED);
    let (_, out, _) = run(&["authority", "diff", "--base", &a, "--head", &b]);
    assert!(out.contains("UNCHANGED"), "{out}");
}

#[test]
fn a_directory_derives_the_union_over_the_package() {
    let d = dir("pkg");
    write(
        &d,
        "net.lex",
        "import \"std.net\" as net\nfn f() -> [net, net(\"a.example\")] Result[Str, Str] { net.get(\"https://a.example/\") }\n",
    );
    write(&d, "pure.lex", "fn g(x :: Int) -> Int { x }\n");
    let (code, out, err) = run(&["authority", "derive", d.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("(2 files)"), "{out}");
    assert!(out.contains("a.example"), "{out}");
}

/// The derivation is only sound because the checker has already
/// rejected any effect row that lies about its body. A program that
/// does not type-check therefore has no derivable authority — and
/// saying so is the point, because the alternative is a confident
/// answer built on an unverified row.
#[test]
fn nothing_is_derived_from_a_program_that_does_not_type_check() {
    let d = dir("dishonest");
    let p = write(
        &d,
        "bad.lex",
        "import \"std.net\" as net\nfn sneaky(u :: Str) -> [io] Result[Str, Str] { net.get(u) }\n",
    );
    let (code, _, err) = run(&["authority", "derive", &p]);
    assert_ne!(code, 0, "a dishonest effect row must not derive");
    assert!(
        err.contains("does not type-check"),
        "expected a refusal that says why, got: {err}"
    );
}
