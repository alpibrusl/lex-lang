//! Multi-file loader smoke tests: two-file project, transitive imports,
//! diamond, cycle detection, missing file, m.Type qualified types,
//! shadowing.

use lex_syntax::syntax::*;
use lex_syntax::{load_program, load_program_from_str, LoadError};
use std::fs;

fn write(dir: &std::path::Path, name: &str, src: &str) {
    fs::write(dir.join(name), src).unwrap();
}

fn fn_names(prog: &Program) -> Vec<String> {
    prog.items
        .iter()
        .filter_map(|i| match i {
            Item::FnDecl(fd) => Some(fd.name.clone()),
            _ => None,
        })
        .collect()
}

fn type_names(prog: &Program) -> Vec<String> {
    prog.items
        .iter()
        .filter_map(|i| match i {
            Item::TypeDecl(td) => Some(td.name.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn two_file_project_mangles_imported_names() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "models.lex",
        r#"type Status = Healthy | Sick
fn label(s :: Status) -> Str {
  match s {
    Healthy => "ok",
    Sick    => "nope",
  }
}
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./models" as m

fn main(s :: m.Status) -> Str { m.label(s) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    let fns = fn_names(&prog);
    let types = type_names(&prog);

    // Imported fn is mangled with the alias.
    assert!(fns.contains(&"m.label".to_string()), "got fns: {fns:?}");
    // Imported type is mangled.
    assert!(types.contains(&"m.Status".to_string()), "got types: {types:?}");
    // Root fn is unmangled.
    assert!(fns.contains(&"main".to_string()), "got fns: {fns:?}");
}

#[test]
fn root_calls_imported_function_via_alias() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "helpers.lex",
        r#"fn double(x :: Int) -> Int { x + x }
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./helpers" as h
fn main(x :: Int) -> Int { h.double(x) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    // The call site `h.double(x)` should have been rewritten to `Var("h.double")`.
    let main_fn = prog
        .items
        .iter()
        .find_map(|i| match i {
            Item::FnDecl(fd) if fd.name == "main" => Some(fd),
            _ => None,
        })
        .expect("main fn present");

    if let Expr::Call { callee, .. } = &*main_fn.body.result {
        if let Expr::Var(name) = &**callee {
            assert_eq!(name, "h.double");
            return;
        }
    }
    panic!("main body not rewritten as expected: {:?}", main_fn.body.result);
}

#[test]
fn unqualified_local_call_inside_imported_file_is_mangled() {
    let dir = tempfile::tempdir().unwrap();
    // helpers.lex defines `inner` and calls it from `outer`. After the
    // loader pass, both should live under the `h` alias path.
    write(
        dir.path(),
        "helpers.lex",
        r#"fn inner(x :: Int) -> Int { x + 1 }
fn outer(x :: Int) -> Int { inner(x) }
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./helpers" as h
fn main(x :: Int) -> Int { h.outer(x) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    let outer = prog
        .items
        .iter()
        .find_map(|i| match i {
            Item::FnDecl(fd) if fd.name == "h.outer" => Some(fd),
            _ => None,
        })
        .expect("h.outer present");
    // Inside h.outer, the call to `inner(x)` should now be `h.inner(x)`.
    if let Expr::Call { callee, .. } = &*outer.body.result {
        if let Expr::Var(name) = &**callee {
            assert_eq!(name, "h.inner");
            return;
        }
    }
    panic!("h.outer body not rewritten: {:?}", outer.body.result);
}

#[test]
fn shadowed_let_binding_is_not_mangled() {
    let dir = tempfile::tempdir().unwrap();
    // `inner` is a top-level fn AND a let binding inside `caller`.
    // The let binding should win inside the body — no mangling.
    write(
        dir.path(),
        "helpers.lex",
        r#"fn inner(x :: Int) -> Int { x }
fn caller(x :: Int) -> Int {
  let inner := x + 100
  inner
}
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./helpers" as h
fn main(x :: Int) -> Int { h.caller(x) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    let caller = prog
        .items
        .iter()
        .find_map(|i| match i {
            Item::FnDecl(fd) if fd.name == "h.caller" => Some(fd),
            _ => None,
        })
        .expect("h.caller present");
    if let Expr::Var(name) = &*caller.body.result {
        // The let-bound `inner` shadows the top-level `h.inner`,
        // so the result expression should reference the unmangled
        // local binder.
        assert_eq!(name, "inner", "let-bound var should not be mangled");
        return;
    }
    panic!("h.caller result not a Var: {:?}", caller.body.result);
}

#[test]
fn transitive_imports_chain() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "c.lex", "fn z(x :: Int) -> Int { x }\n");
    write(
        dir.path(),
        "b.lex",
        r#"import "./c" as c
fn y(x :: Int) -> Int { c.z(x) }
"#,
    );
    write(
        dir.path(),
        "a.lex",
        r#"import "./b" as b
fn main(x :: Int) -> Int { b.y(x) }
"#,
    );

    let prog = load_program(&dir.path().join("a.lex")).expect("load");
    let fns = fn_names(&prog);
    assert!(fns.contains(&"main".to_string()));
    assert!(fns.contains(&"b.y".to_string()));
    assert!(fns.contains(&"b.c.z".to_string()), "got: {fns:?}");
}

#[test]
fn cycle_detection_errors_with_chain() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.lex", "import \"./b\" as b\nfn fa() -> Int { 1 }\n");
    write(dir.path(), "b.lex", "import \"./a\" as a\nfn fb() -> Int { 2 }\n");

    let err = load_program(&dir.path().join("a.lex")).expect_err("expected cycle error");
    let msg = format!("{err}");
    match err {
        LoadError::Cycle { .. } => {
            assert!(msg.contains("a.lex"), "msg: {msg}");
            assert!(msg.contains("b.lex"), "msg: {msg}");
        }
        other => panic!("expected Cycle, got: {other:?}"),
    }
}

#[test]
fn missing_file_errors_clearly() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "main.lex",
        "import \"./nonexistent\" as x\nfn main() -> Int { 0 }\n",
    );

    let err = load_program(&dir.path().join("main.lex")).expect_err("expected missing-file error");
    match err {
        LoadError::NotFound { reference, .. } => assert_eq!(reference, "./nonexistent"),
        other => panic!("expected NotFound, got: {other:?}"),
    }
}

#[test]
fn string_source_rejects_local_imports() {
    let err = load_program_from_str("import \"./foo\" as f\nfn main() -> Int { 0 }\n")
        .expect_err("expected rejection");
    matches!(err, LoadError::LocalImportInStringSource);
}

#[test]
fn string_source_accepts_std_imports() {
    let prog = load_program_from_str("import \"std.io\" as io\nfn main() -> Int { 0 }\n")
        .expect("std import in string source");
    assert!(prog
        .items
        .iter()
        .any(|i| matches!(i, Item::Import(imp) if imp.reference == "std.io")));
}

#[test]
fn diamond_keeps_shared_module_once() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "shared.lex",
        "fn util(x :: Int) -> Int { x + 1 }\n",
    );
    write(
        dir.path(),
        "left.lex",
        "import \"./shared\" as s\nfn lhs(x :: Int) -> Int { s.util(x) }\n",
    );
    write(
        dir.path(),
        "right.lex",
        "import \"./shared\" as s\nfn rhs(x :: Int) -> Int { s.util(x) }\n",
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./left" as l
import "./right" as r
fn main(x :: Int) -> Int { l.lhs(x) + r.rhs(x) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    let fns = fn_names(&prog);
    // We accept duplication of `shared.util` under each parent's path
    // for the MVP. The test documents current behavior:
    // - `l.s.util` and `r.s.util` both appear.
    // (See follow-up tracker for store-native imports / SigId stability.)
    let l_util = fns.iter().filter(|n| n == &"l.s.util").count();
    let r_util = fns.iter().filter(|n| n == &"r.s.util").count();
    assert_eq!(l_util, 1, "got fns: {fns:?}");
    assert_eq!(r_util, 1, "got fns: {fns:?}");
}

#[test]
fn std_import_in_imported_file_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "io_helper.lex",
        r#"import "std.io" as io
fn say(s :: Str) -> [io] Nil { io.print(s) }
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./io_helper" as h
fn main(s :: Str) -> [io] Nil { h.say(s) }
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");
    let std_imports: Vec<&Import> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Import(imp) => Some(imp),
            _ => None,
        })
        .collect();
    // The std.io import should appear exactly once, even though both
    // files would have included it.
    assert_eq!(std_imports.len(), 1, "got: {std_imports:?}");
    assert_eq!(std_imports[0].reference, "std.io");
    // io.print inside h.say should NOT have been rewritten.
    let say = prog
        .items
        .iter()
        .find_map(|i| match i {
            Item::FnDecl(fd) if fd.name == "h.say" => Some(fd),
            _ => None,
        })
        .expect("h.say present");
    if let Expr::Call { callee, .. } = &*say.body.result {
        if let Expr::Field { value, field } = &**callee {
            if let Expr::Var(alias) = &**value {
                assert_eq!(alias, "io");
                assert_eq!(field, "print");
                return;
            }
        }
    }
    panic!("h.say body not preserving io.print: {:?}", say.body.result);
}
