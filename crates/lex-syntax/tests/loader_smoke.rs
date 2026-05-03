//! Multi-file loader smoke tests: two-file project, transitive imports,
//! diamond (now deduped), cycle detection, missing file, m.Type
//! qualified types, shadowing.

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

/// Find the unique fn whose mangled name ends with `.<suffix>` (an
/// imported fn) or equals `<suffix>` (a root-file fn). Asserts
/// uniqueness.
fn unique_fn<'a>(prog: &'a Program, suffix: &str) -> &'a FnDecl {
    let matches: Vec<&FnDecl> = prog
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FnDecl(fd) if fd.name == suffix || fd.name.ends_with(&format!(".{suffix}")) => {
                Some(fd)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one fn matching `{suffix}`, found {}: {:?}",
        matches.len(),
        matches.iter().map(|f| &f.name).collect::<Vec<_>>(),
    );
    matches[0]
}

fn count_with_suffix(prog: &Program, suffix: &str) -> usize {
    prog.items
        .iter()
        .filter(|i| match i {
            Item::FnDecl(fd) => fd.name == suffix || fd.name.ends_with(&format!(".{suffix}")),
            _ => false,
        })
        .count()
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

    // Imported fn is mangled (some prefix, ending in .label).
    assert!(
        fns.iter().any(|n| n.ends_with(".label") && n.contains('_')),
        "expected mangled fn ending in `.label` with `_` separator, got: {fns:?}",
    );
    // Imported type likewise.
    assert!(
        types.iter().any(|n| n.ends_with(".Status") && n.contains('_')),
        "expected mangled type ending in `.Status`, got: {types:?}",
    );
    // Root fn stays unmangled.
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
    let main_fn = unique_fn(&prog, "main");
    let imported = unique_fn(&prog, "double");

    if let Expr::Call { callee, .. } = &*main_fn.body.result {
        if let Expr::Var(name) = &**callee {
            assert_eq!(
                name, &imported.name,
                "main's call should reference the imported fn's mangled name"
            );
            return;
        }
    }
    panic!("main body not rewritten as expected: {:?}", main_fn.body.result);
}

#[test]
fn unqualified_local_call_inside_imported_file_is_mangled() {
    let dir = tempfile::tempdir().unwrap();
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
    let outer = unique_fn(&prog, "outer");
    let inner = unique_fn(&prog, "inner");

    if let Expr::Call { callee, .. } = &*outer.body.result {
        if let Expr::Var(name) = &**callee {
            assert_eq!(name, &inner.name, "outer's body should call inner via mangled name");
            return;
        }
    }
    panic!("outer body not rewritten: {:?}", outer.body.result);
}

#[test]
fn shadowed_let_binding_is_not_mangled() {
    let dir = tempfile::tempdir().unwrap();
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
    let caller = unique_fn(&prog, "caller");
    if let Expr::Var(name) = &*caller.body.result {
        assert_eq!(name, "inner", "let-bound var should not be mangled");
        return;
    }
    panic!("caller result not a Var: {:?}", caller.body.result);
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
    assert_eq!(count_with_suffix(&prog, "y"), 1);
    assert_eq!(count_with_suffix(&prog, "z"), 1);
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
fn diamond_imports_share_one_module_identity() {
    // Closes #88: two parents importing the same file produce one
    // (deduped) set of mangled items, so `s.util` and `r.util` both
    // resolve to the same fn under the same mangled name.
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

    // util appears exactly once under its mangled name (regardless of
    // which parent's alias chain we'd have walked).
    assert_eq!(
        count_with_suffix(&prog, "util"),
        1,
        "shared.util should appear once after dedupe; got fns: {:?}",
        fn_names(&prog),
    );

    // Both lhs and rhs call sites resolve to that same name.
    let util = unique_fn(&prog, "util");
    let lhs = unique_fn(&prog, "lhs");
    let rhs = unique_fn(&prog, "rhs");

    fn callee_name(body: &Block) -> &str {
        if let Expr::Call { callee, .. } = &*body.result {
            if let Expr::Var(name) = &**callee {
                return name;
            }
        }
        panic!("body result not a Call(Var): {:?}", body.result);
    }
    assert_eq!(callee_name(&lhs.body), util.name);
    assert_eq!(callee_name(&rhs.body), util.name);
}

#[test]
fn diamond_with_imported_type_unifies_across_branches() {
    // The user's repro from #88: scorer builds Report, verdict
    // consumes Report, both reach Report via different aliases.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "models.lex", "type Report = { score :: Int }\n");
    write(
        dir.path(),
        "scorer.lex",
        "import \"./models\" as m\nfn build_report(s :: Int) -> m.Report { { score: s } }\n",
    );
    write(
        dir.path(),
        "verdict.lex",
        "import \"./models\" as m\nfn read_score(r :: m.Report) -> Int { r.score }\n",
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./scorer" as s
import "./verdict" as v

fn main() -> Int {
  let r := s.build_report(7)
  v.read_score(r)
}
"#,
    );

    let prog = load_program(&dir.path().join("main.lex")).expect("load");

    // The build_report and read_score signatures should reference the
    // same mangled `Report` name, so a downstream type-check unifies.
    let builder = unique_fn(&prog, "build_report");
    let reader = unique_fn(&prog, "read_score");

    let builder_ret = match &builder.return_type {
        TypeExpr::Named { name, .. } => name.clone(),
        other => panic!("expected Named return type, got {other:?}"),
    };
    let reader_param = match &reader.params[0].ty {
        TypeExpr::Named { name, .. } => name.clone(),
        other => panic!("expected Named param type, got {other:?}"),
    };
    assert_eq!(
        builder_ret, reader_param,
        "diamond branches should resolve to the same nominal type",
    );
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
    assert_eq!(std_imports.len(), 1, "got: {std_imports:?}");
    assert_eq!(std_imports[0].reference, "std.io");

    // io.print inside say should NOT have been rewritten.
    let say = unique_fn(&prog, "say");
    if let Expr::Call { callee, .. } = &*say.body.result {
        if let Expr::Field { value, field } = &**callee {
            if let Expr::Var(alias) = &**value {
                assert_eq!(alias, "io");
                assert_eq!(field, "print");
                return;
            }
        }
    }
    panic!("say body not preserving io.print: {:?}", say.body.result);
}
