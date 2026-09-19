//! #980: replay must work for a **package** publish.
//!
//! A package publish mangles every declaration with a path-derived prefix
//! (`lib_<hash>.twice`). That prefix is a storage detail — a dotted name is not
//! valid Lex source at all:
//!
//! ```text
//! $ echo 'fn lib_abc.twice(n :: Int) -> Int { n * 2 }' > m.lex && lex check m.lex
//! error: parse error at byte 13: expected LParen before params, got Dot
//! ```
//!
//! So the old `target_signature` asked a regenerator for something unwritable,
//! and a regenerator that sensibly emitted the bare `fn twice(..)` was rejected
//! anyway, because the comparison required exact name equality. Every
//! package-published op therefore replayed as a **false negative** — recorded
//! as "did not reproduce" even for a perfect regeneration.
//!
//! Note these cases involve **no dependencies**: the bug is entirely about
//! name mangling, which is what distinguishes it from #946.

use std::path::Path;
use std::process::Command;

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn write(dir: &Path, name: &str, src: &str) -> String {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, src).unwrap();
    p.to_string_lossy().into_owned()
}

fn json_in(cwd: &Path, args: &[&str]) -> serde_json::Value {
    let out = lex()
        .current_dir(cwd)
        .args(["--output", "json"])
        .args(args)
        .output()
        .expect("run lex");
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!(
            "non-JSON from lex {args:?}: {e}\nstdout: {text}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

/// A one-file package publish (`lex publish <dir>`), which mangles names.
fn publish_package(dir: &Path, body: &str) -> (String, String) {
    let app = dir.join("app");
    write(&app, "lex.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
    write(&app.join("src"), "lib.lex", body);
    let store = app.join(".lex/store").to_string_lossy().into_owned();
    let v = json_in(&app, &["publish", ".", "--store", &store, "--activate"]);
    let op = data(&v)["ops"][0]["op_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no op published: {v}"))
        .to_string();
    (op, store)
}

const TWICE: &str = "fn twice(n :: Int) -> Int\n  \
    examples {\n    twice(2) => 4\n  }\n\
    {\n  match n { 0 => 0, _ => n * 2 }\n}\n";

#[test]
fn the_replay_request_asks_for_source_that_can_actually_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let (op, store) = publish_package(dir.path(), TWICE);
    let app = dir.path().join("app");

    let req = json_in(&app, &["op", "replay", &op, "--store", &store]);
    let rd = data(&req);
    let name = rd["target_name"].as_str().unwrap_or_default();
    let sig = rd["target_signature"].as_str().unwrap_or_default();

    assert_eq!(name, "twice", "the request must name the function as its author wrote it: {req}");
    assert!(
        !name.contains('.') && !sig.contains('.'),
        "a mangled (dotted) name cannot be written as Lex source, so the request \
         must not ask for one: name={name:?} sig={sig:?}"
    );

    // The strong form: what the request asks for must actually parse.
    let probe = write(dir.path(), "probe.lex", &format!("{sig} {{ n * 2 }}\n"));
    let out = lex().args(["check", &probe]).output().unwrap();
    assert!(
        out.status.success(),
        "the requested signature must be valid source, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // And the parent program shown as context is de-mangled too.
    let parent = rd["parent_program"].as_str().unwrap_or_default();
    assert!(
        !parent.contains("lib_"),
        "parent context must be de-mangled source, got:\n{parent}"
    );
}

#[test]
fn a_bare_named_regeneration_of_a_package_op_reproduces() {
    let dir = tempfile::tempdir().unwrap();
    let (op, store) = publish_package(dir.path(), TWICE);
    let app = dir.path().join("app");

    // Exactly what a regenerator following the request would emit: the bare
    // name, same function, written differently (`if` rather than `match`), so
    // the behavioral tier is the one that has to answer.
    let cand = write(
        dir.path(),
        "cand.lex",
        "fn twice(n :: Int) -> Int { if n == 0 { 0 } else { n * 2 } }\n",
    );

    let r = json_in(&app, &["op", "replay", &op, "--store", &store, "--candidate", &cand]);
    let rd = data(&r);
    assert_eq!(
        rd["reproduced"].as_bool(),
        Some(true),
        "a correct bare-named regeneration of a package-published op must reproduce, \
         not be rejected for lacking the storage prefix: {r}"
    );
}

/// The exact tier must work too: a byte-identical regeneration (same body AND
/// the recorded examples) reproduces *exactly*, not merely behaviorally.
#[test]
fn an_identical_regeneration_reproduces_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let (op, store) = publish_package(dir.path(), TWICE);
    let app = dir.path().join("app");

    let cand = write(dir.path(), "same.lex", TWICE);
    let r = json_in(&app, &["op", "replay", &op, "--store", &store, "--candidate", &cand]);
    let rd = data(&r);
    assert_eq!(rd["reproduced"].as_bool(), Some(true), "{r}");
    assert!(
        rd["behavioral_samples"].is_null(),
        "an identical regeneration must count as EXACT, not fall through to the \
         behavioral tier: {r}"
    );
}
