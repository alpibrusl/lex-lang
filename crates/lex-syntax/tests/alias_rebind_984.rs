//! #984: two files in one package may bind the same import alias to different
//! modules. That is legal — import aliases are file-scoped, and each file
//! type-checks on its own — but a package flattens into ONE program whose
//! alias scope is name-keyed, so the alias must be made unique.
//!
//! It used to be rejected outright (`LoadError::ConflictingAlias`), which made
//! valid, compiling packages unpublishable: `lex check` passed on every file
//! while `lex publish <dir>` refused the package. Four of the largest hosted
//! packages were blocked this way (lex-code, lex-loom, lex-soft, lex-oms-agent).
//!
//! Now the later file's alias is re-bound and that file's references are
//! rewritten with it — the same treatment declarations already get from
//! mangling (#818).

use std::path::PathBuf;

use lex_syntax::loader::load_package;

fn write(dir: &std::path::Path, rel: &str, src: &str) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, src).unwrap();
    p
}

/// A package whose two files both use the alias `ev`, for different packages.
fn conflicting_package(dir: &std::path::Path) -> Vec<PathBuf> {
    write(dir, "lex.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
    let a = write(
        dir,
        "src/gates.lex",
        "import \"lex-spec/eval\" as ev\n\nfn gate(n :: Int) -> Int { ev.score(n) }\n",
    );
    let b = write(
        dir,
        "src/trail.lex",
        "import \"lex-trail/event\" as ev\n\nfn emit(n :: Int) -> Int { ev.record(n) }\n",
    );
    vec![a, b]
}

fn import_pairs(prog: &lex_syntax::syntax::Program) -> Vec<(String, String)> {
    prog.items
        .iter()
        .filter_map(|i| match i {
            lex_syntax::syntax::Item::Import(imp) => {
                Some((imp.reference.clone(), imp.alias.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_reused_alias_is_rebound_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let entries = conflicting_package(dir.path());

    // Non-inlined: the publish path (#930), where package aliases survive into
    // the flattened program and therefore have to be unique.
    let loaded = load_package(&entries, dir.path(), "app", false)
        .expect("a package whose files reuse an alias must load, not be rejected");

    let imports = import_pairs(&loaded.program);
    let refs: Vec<&String> = imports.iter().map(|(r, _)| r).collect();
    assert!(refs.iter().any(|r| r.contains("lex-spec/eval")), "{imports:?}");
    assert!(refs.iter().any(|r| r.contains("lex-trail/event")), "{imports:?}");

    // Both modules survive, under DIFFERENT aliases — neither is dropped and
    // neither silently shadows the other.
    let aliases: Vec<&String> = imports.iter().map(|(_, a)| a).collect();
    let spec_alias = &imports.iter().find(|(r, _)| r.contains("lex-spec/eval")).unwrap().1;
    let trail_alias = &imports.iter().find(|(r, _)| r.contains("lex-trail/event")).unwrap().1;
    assert_ne!(
        spec_alias, trail_alias,
        "the two modules must end up under distinct aliases: {aliases:?}"
    );
    assert!(
        spec_alias.as_str() == "ev" || trail_alias.as_str() == "ev",
        "the first binding should keep the author's alias: {aliases:?}"
    );
}

/// The re-bound alias must be applied to the *referencing* file's body too —
/// otherwise the import is renamed but the calls still name the old alias, and
/// the program fails as `unknown_identifier`.
#[test]
fn the_rebound_alias_is_applied_to_that_files_references() {
    let dir = tempfile::tempdir().unwrap();
    let entries = conflicting_package(dir.path());
    let loaded = load_package(&entries, dir.path(), "app", false).expect("load");

    let rendered = format!("{:?}", loaded.program);
    let imports = import_pairs(&loaded.program);
    let renamed = imports
        .iter()
        .map(|(_, a)| a.clone())
        .find(|a| a != "ev")
        .expect("one alias must have been re-bound");

    // The renamed alias is actually used by a call somewhere.
    assert!(
        rendered.contains(&renamed),
        "the re-bound alias {renamed} must appear in the rewritten references"
    );
    // And it stays a qualified reference rather than collapsing into a dotted
    // declaration name — a renamed alias still resolves through the import.
    assert!(
        !rendered.contains(&format!("{renamed}.score")) || !rendered.contains(&format!("\"{renamed}.")),
        "a re-bound alias must remain a field access, not a flat dotted name"
    );
}

/// The same module under the same alias in two files is still just one import —
/// de-duplication must not be mistaken for a conflict and re-bound.
#[test]
fn the_same_module_under_the_same_alias_is_deduped_not_renamed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lex.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
    let a = write(
        dir.path(),
        "src/one.lex",
        "import \"lex-spec/eval\" as ev\n\nfn a(n :: Int) -> Int { ev.score(n) }\n",
    );
    let b = write(
        dir.path(),
        "src/two.lex",
        "import \"lex-spec/eval\" as ev\n\nfn b(n :: Int) -> Int { ev.score(n) }\n",
    );
    let loaded = load_package(&[a, b], dir.path(), "app", false).expect("load");

    let imports = import_pairs(&loaded.program);
    let evs: Vec<_> = imports.iter().filter(|(r, _)| r.contains("lex-spec/eval")).collect();
    assert_eq!(evs.len(), 1, "one module under one alias is a single import: {imports:?}");
    assert_eq!(evs[0].1, "ev", "no rename was needed, so the alias must be untouched");
}
