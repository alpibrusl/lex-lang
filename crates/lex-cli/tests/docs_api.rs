//! `lex docs <path>` — structured API docs from source (#564). Emits
//! per-module / per-function JSON (name, sig_id, signature, effects,
//! examples, doc) consumable by a static site renderer or a doc-gen
//! agent.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).to_string())
}

/// Build a temp package: `lex.toml` + `src/math.lex`. Returns the dir.
fn seed_pkg() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lex.toml"),
        "[package]\nname = \"demo-pkg\"\nversion = \"0.3.1\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    // First fn carries an `examples {}` block and a per-fn doc comment;
    // the file opens with a module-level comment; the second fn is
    // effectful so we can assert the effect row is surfaced.
    std::fs::write(
        dir.path().join("src/math.lex"),
        "# Arithmetic helpers.\n\
         import \"std.io\" as io\n\n\
         # Add two integers.\n\
         # Pure and total.\n\
         fn add(x :: Int, y :: Int) -> Int\n\
         \x20\x20examples { add(2, 3) => 5, add(0, 0) => 0 }\n\
         { x + y }\n\n\
         fn shout(s :: Str) -> [io] Nil {\n\
         \x20\x20io.print(s)\n\
         }\n",
    )
    .unwrap();
    dir
}

#[test]
fn api_docs_json_has_package_modules_and_functions() {
    let dir = seed_pkg();
    let src = dir.path().join("src");
    let (code, stdout) = run(&["--output", "json", "docs", src.to_str().unwrap()]);
    assert_eq!(code, 0, "docs should succeed: {stdout}");

    let env: serde_json::Value = serde_json::from_str(&stdout).expect("envelope parses");
    let data = &env["data"];
    assert_eq!(data["lex_docs_version"], 1);
    assert_eq!(data["package"], "demo-pkg");
    assert_eq!(data["version"], "0.3.1");

    let modules = data["modules"].as_array().expect("modules array");
    assert_eq!(modules.len(), 1, "one .lex file → one module: {data}");
    let m = &modules[0];
    assert!(m["file"].as_str().unwrap().ends_with("math.lex"));
    // Module-level comment (top of file) surfaces as the module doc.
    assert_eq!(m["doc"], "Arithmetic helpers.");

    let fns = m["functions"].as_array().expect("functions array");
    let add = fns.iter().find(|f| f["name"] == "add").expect("add present");
    assert!(add["sig_id"].as_str().is_some_and(|s| s.len() == 64), "sig_id is a hash: {add}");
    assert_eq!(add["signature"], "(x :: Int, y :: Int) -> Int");
    assert!(add["effects"].as_array().unwrap().is_empty(), "add is pure");
    let examples: Vec<&str> = add["examples"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
    assert_eq!(examples, vec!["add(2, 3) => 5", "add(0, 0) => 0"]);
    assert_eq!(add["doc"], "Add two integers.\nPure and total.");

    let shout = fns.iter().find(|f| f["name"] == "shout").expect("shout present");
    assert_eq!(shout["effects"].as_array().unwrap(), &vec![serde_json::json!("io")]);
}

#[test]
fn api_docs_sig_id_matches_workspace_sig_id() {
    // The sig_id emitted by `lex docs` must equal the one the rest of
    // the toolchain computes for the same source, so docs can be keyed
    // by SigId across renames/reformats.
    let dir = seed_pkg();
    let src = dir.path().join("src/math.lex");
    let (code, stdout) = run(&["--output", "json", "docs", src.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}");
    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let add = env["data"]["modules"][0]["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "add")
        .unwrap()
        .clone();

    let source = std::fs::read_to_string(&src).unwrap();
    let prog = lex_syntax::parse_source(&source).unwrap();
    let stages = lex_ast::canonicalize_program(&prog);
    let stage = stages
        .iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == "add"))
        .unwrap();
    assert_eq!(add["sig_id"].as_str().unwrap(), lex_ast::sig_id(stage).unwrap());
}

#[test]
fn api_docs_text_mode_succeeds() {
    let dir = seed_pkg();
    let (code, stdout) = run(&["docs", dir.path().join("src").to_str().unwrap()]);
    assert_eq!(code, 0, "text docs should succeed: {stdout}");
    assert!(stdout.contains("demo-pkg 0.3.1"), "text render names the package: {stdout}");
    assert!(stdout.contains("add"), "text render lists fns: {stdout}");
}

#[test]
fn unknown_docs_flag_is_rejected() {
    let (code, _stdout) = run(&["docs", "--bogus"]);
    assert_ne!(code, 0, "unknown --flag should be rejected, not treated as a path");
}
