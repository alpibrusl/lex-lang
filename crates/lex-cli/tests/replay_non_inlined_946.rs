//! #946 step 2: `lex op replay` can behaviorally replay a **non-inlined** head.
//!
//! Since #930 a head keeps `import "<pkg>/<module>" as <alias>` edges instead
//! of inlining the dependency's code. That head type-checks (the resolver
//! supplies signatures) but could not be *executed*: the implementations are
//! not in the op-log, so the behavioral tier's `compiled()` failed and every
//! regeneration of a dependency-using function was recorded as "did not
//! reproduce" — even a perfect one.
//!
//! `compiled()` now links first, re-materializing the head as source and
//! loading it through the same inlining loader `lex run` uses.
//!
//! The candidate here is deliberately the *same function written differently*
//! (`if` instead of `match`), so the exact stage-id match fails and the run
//! must fall through to the behavioral tier — which is the tier that needs to
//! execute, and therefore the tier that needed linking.

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

#[test]
fn a_non_inlined_head_replays_behaviorally() {
    let dir = tempfile::tempdir().unwrap();
    // A dependency package, and an app that imports it via a path dep — the
    // same layout the loader's own package tests use.
    let dep = dir.path().join("lex-math");
    write(&dep, "lex.toml", "[package]\nname = \"lex-math\"\nversion = \"0.1.0\"\n");
    write(
        &dep.join("src"),
        "arith.lex",
        "fn clamp_low(n :: Int) -> Int { match n { 0 => 0, _ => n } }\n",
    );

    let app = dir.path().join("app");
    write(
        &app,
        "lex.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nlex-math = { path = \"../lex-math\" }\n",
    );
    // `scale` calls through the dependency alias, so its head cannot run
    // without the dependency's implementation.
    write(
        &app.join("src"),
        "lib.lex",
        "import \"lex-math/arith\" as m\n\n\
         fn scale(n :: Int) -> Int\n  \
           examples {\n    scale(0) => 0,\n    scale(3) => 6\n  }\n\
         {\n  match n { 0 => 0, _ => m.clamp_low(n) * 2 }\n}\n",
    );

    let store = app.join(".lex/store").to_string_lossy().into_owned();
    let v = json_in(&app, &["publish", ".", "--store", &store, "--activate"]);
    let d = data(&v);
    let op_id = d["ops"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|o| o.get("op").and_then(|k| k.as_str()) == Some("add_function"))
                .or_else(|| a.last())
        })
        .and_then(|o| o["op_id"].as_str())
        .unwrap_or_else(|| panic!("no op published: {v}"))
        .to_string();

    // The same function, written differently: `if` rather than `match`. Not
    // byte-identical, so the exact tier must miss and the behavioral tier run.
    let cand = write(
        dir.path(),
        "cand.lex",
        "import \"lex-math/arith\" as m\n\n\
         fn scale(n :: Int) -> Int {\n  \
           if n == 0 { 0 } else { m.clamp_low(n) * 2 }\n\
         }\n",
    );

    let r = json_in(&app, &["op", "replay", &op_id, "--store", &store, "--candidate", &cand]);
    let rd = data(&r);

    assert_eq!(
        rd["reproduced"].as_bool(),
        Some(true),
        "a non-inlined head must be executable so an equivalent regeneration \
         reproduces behaviorally: {r}"
    );
    assert!(
        rd["behavioral_samples"].as_u64().unwrap_or(0) > 0,
        "it must reproduce via the BEHAVIORAL tier (the one that executes), \
         not an exact stage match: {r}"
    );
}

/// The linker must leave a dependency-free head exactly as it was: nothing to
/// resolve, no package context needed, same verdict as before #946.
#[test]
fn a_dependency_free_head_is_unaffected() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("plain");
    write(&app, "lex.toml", "[package]\nname = \"plain\"\nversion = \"0.1.0\"\n");
    write(
        &app.join("src"),
        "lib.lex",
        "fn twice(n :: Int) -> Int\n  \
           examples {\n    twice(2) => 4\n  }\n\
         {\n  match n { 0 => 0, _ => n * 2 }\n}\n",
    );

    let store = app.join(".lex/store").to_string_lossy().into_owned();
    let v = json_in(&app, &["publish", ".", "--store", &store, "--activate"]);
    let op_id = data(&v)["ops"][0]["op_id"].as_str().expect("op id").to_string();

    let cand = write(
        dir.path(),
        "plain_cand.lex",
        "fn twice(n :: Int) -> Int { if n == 0 { 0 } else { n * 2 } }\n",
    );
    let r = json_in(&app, &["op", "replay", &op_id, "--store", &store, "--candidate", &cand]);
    let rd = data(&r);
    assert_eq!(rd["reproduced"].as_bool(), Some(true), "{r}");
}
