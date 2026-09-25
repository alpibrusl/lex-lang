//! #909: `lex publish <pkg>` then `lex export-git` reproduces a local import's
//! alias — `import "./error" as e` — verbatim, where it used to come back as
//! `as error` (the target file's stem).
//!
//! Also drives the consumers of an `AddImport` whose module is a *path* through
//! the real binary: `lex op replay` on a package with aliased local imports must
//! still work, and republishing must not churn the log.

use std::path::Path;
use std::process::Command;

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn write(dir: &Path, name: &str, src: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, src).unwrap();
}

fn json(args: &[&str]) -> serde_json::Value {
    let out = lex().args(["--output", "json"]).args(args).output().expect("run lex");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "lex {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("non-JSON from lex {args:?}: {e}\n{text}"))
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

const ERROR: &str = "type Err = { code :: Int, msg :: Str }\n\n\
    fn format(e :: Err) -> Str {\n  e.msg\n}\n";

/// The issue's shape: `json_value` imports `./error` as `e`; `lib` imports it
/// under its default alias (the control) and `json_value` as `jv`.
const JSON_VALUE: &str = "import \"./error\" as e\n\n\
    fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n";
const LIB: &str = "import \"./error\" as error\n\n\
    import \"./json_value\" as jv\n\n\
    fn top(x :: error.Err) -> Str {\n  jv.render(x)\n}\n";

fn write_package(pkg: &Path, files: &[(&str, &str)]) {
    write(pkg, "lex.toml", "[package]\nname = \"fxpkg\"\nversion = \"0.1.0\"\n");
    for (name, body) in files {
        write(&pkg.join("src"), name, body);
    }
}

fn publish(pkg: &Path, store: &Path) -> serde_json::Value {
    json(&["publish", pkg.to_str().unwrap(), "--store", store.to_str().unwrap(), "--activate"])
}

fn export(store: &Path, out: &Path) {
    let res = lex()
        .args(["export-git", out.to_str().unwrap(), "--store", store.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// `(in_file, module, alias)` of every `add_import` in the branch's op log.
fn add_imports(store: &Path) -> Vec<(String, String, Option<String>)> {
    let v = json(&["op", "log", "--store", store.to_str().unwrap()]);
    data(&v)["log"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["op"] == "add_import")
        .map(|o| {
            (
                o["in_file"].as_str().unwrap().to_string(),
                o["module"].as_str().unwrap().to_string(),
                o.get("alias").and_then(|a| a.as_str()).map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn export_git_reproduces_a_non_default_local_alias_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let (pkg, store, out) = (dir.path().join("pkg"), dir.path().join("store"), dir.path().join("out"));
    write_package(
        &pkg,
        &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE), ("lib.lex", LIB)],
    );
    publish(&pkg, &store);
    export(&store, &out);

    let jv = read(&out, "src/json_value.lex");
    assert!(jv.contains("import \"./error\" as e\n"), "alias `e` must survive:\n{jv}");
    assert!(jv.contains("e.format(x)"), "the call site keeps `e`:\n{jv}");
    assert!(!jv.contains("as error"), "must not be re-derived from the stem:\n{jv}");
    assert_eq!(jv, JSON_VALUE, "verbatim");

    // Control: the default alias round-trips unchanged.
    assert_eq!(read(&out, "src/lib.lex"), LIB);
    assert_eq!(read(&out, "src/error.lex"), ERROR);

    // ...and the recorded ops have the exact shape: the alias only where it is
    // not the default.
    let mut ops = add_imports(&store);
    ops.sort();
    assert_eq!(
        ops,
        vec![
            ("src/json_value.lex".into(), "./error".into(), Some("e".into())),
            ("src/lib.lex".into(), "./error".into(), None),
            ("src/lib.lex".into(), "./json_value".into(), Some("jv".into())),
        ]
    );

    // The exported tree is a compilable package.
    std::fs::copy(pkg.join("lex.toml"), out.join("lex.toml")).unwrap();
    for f in ["src/error.lex", "src/json_value.lex", "src/lib.lex"] {
        let c = lex().args(["check", out.join(f).to_str().unwrap()]).output().unwrap();
        assert!(c.status.success(), "{f} must type-check: {}", String::from_utf8_lossy(&c.stderr));
    }
}

#[test]
fn an_unchanged_republish_creates_no_ops_and_a_realias_is_reflected() {
    let dir = tempfile::tempdir().unwrap();
    let (pkg, store, out) = (dir.path().join("pkg"), dir.path().join("store"), dir.path().join("out"));
    write_package(&pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    publish(&pkg, &store);

    let again = publish(&pkg, &store);
    assert_eq!(
        data(&again)["ops"].as_array().map(Vec::len),
        Some(0),
        "unchanged republish must create 0 ops: {again}"
    );

    // Re-alias: only the import pair is emitted, and the export follows.
    let renamed = "import \"./error\" as err\n\n\
        fn render(x :: err.Err) -> Str {\n  err.format(x)\n}\n";
    write(&pkg.join("src"), "json_value.lex", renamed);
    let r = publish(&pkg, &store);
    let kinds: Vec<&str> = data(&r)["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"]["op"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["remove_import", "add_import"], "{r}");
    export(&store, &out);
    assert_eq!(read(&out, "src/json_value.lex"), renamed);
}

/// `lex op replay` on a package whose files import each other under aliases.
///
/// Two shapes: the replay *request* for an op in a multi-file head (the head is
/// reconstructed, and must not carry the local import as though it were a
/// dependency), and a full behavioral replay of a function in a head whose
/// declarations live in one module — the case replay can execute end to end —
/// while another file imports that module under an alias.
#[test]
fn op_replay_still_works_on_a_package_with_aliased_local_imports() {
    let dir = tempfile::tempdir().unwrap();
    let (pkg, store) = (dir.path().join("pkg"), dir.path().join("store"));

    // (1) multi-file: the request builds and the parent program is clean.
    write_package(&pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    publish(&pkg, &store);
    let log = json(&["op", "log", "--store", store.to_str().unwrap()]);
    let render_op = data(&log)["log"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op"] == "add_function" && o["in_file"] == "src/json_value.lex")
        .expect("render's add_function")["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    let req = json(&["op", "replay", &render_op, "--store", store.to_str().unwrap()]);
    let parent = data(&req)["parent_program"].as_str().unwrap_or_default();
    assert!(!parent.contains("./error"), "parent program leaked a local import:\n{parent}");

    // (2) one module holds the declarations; a barrel file aliases it.
    let pkg2 = dir.path().join("pkg2");
    let store2 = dir.path().join("store2");
    let twice = "fn twice(n :: Int) -> Int\n  examples {\n    twice(2) => 4\n  }\n\
        {\n  match n { 0 => 0, _ => n * 2 }\n}\n";
    write_package(&pkg2, &[("core.lex", twice), ("lib.lex", "import \"./core\" as c\n")]);
    publish(&pkg2, &store2);
    assert!(
        add_imports(&store2).contains(&("src/lib.lex".into(), "./core".into(), Some("c".into()))),
        "the barrel's aliased import is recorded: {:?}",
        add_imports(&store2)
    );
    let log2 = json(&["op", "log", "--store", store2.to_str().unwrap()]);
    let twice_op = data(&log2)["log"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op"] == "add_function")
        .expect("twice's add_function")["op_id"]
        .as_str()
        .unwrap()
        .to_string();
    // A regeneration written differently (`if` for `match`) must be judged by
    // the behavioral tier — which links and executes the reconstructed head.
    write(dir.path(), "cand.lex", "fn twice(n :: Int) -> Int { if n == 0 { 0 } else { n * 2 } }\n");
    let r = json(&[
        "op", "replay", &twice_op, "--store", store2.to_str().unwrap(),
        "--candidate", dir.path().join("cand.lex").to_str().unwrap(),
    ]);
    assert_eq!(data(&r)["reproduced"].as_bool(), Some(true), "{r}");
}
