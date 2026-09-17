//! `lex propagate` end to end (#893, parts b + c): mechanical rename fan-out
//! over a workspace, and agent-gated semantic migration.

use std::path::Path;
use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

/// Build a workspace: an upstream package `up` and a dependent `dn` that
/// imports it and calls `up.old`.
fn workspace(dir: &Path) {
    let up = dir.join("up");
    let dn = dir.join("dn");
    std::fs::create_dir_all(up.join("src")).unwrap();
    std::fs::create_dir_all(dn.join("src")).unwrap();
    std::fs::write(up.join("lex.toml"), "[package]\nname = \"up\"\nversion = \"1.0.0\"\n").unwrap();
    std::fs::write(up.join("src/lib.lex"), "fn old(x :: Int) -> Int { x + 1 }\n").unwrap();
    std::fs::write(
        dn.join("lex.toml"),
        "[package]\nname = \"dn\"\nversion = \"0.1.0\"\n\n[dependencies]\nup = { path = \"../up\" }\n",
    )
    .unwrap();
    std::fs::write(
        dn.join("src/lib.lex"),
        "import \"up/lib\" as up\n\n# uses the helper\nfn use_it(x :: Int) -> Int { up.old(x) }\n",
    )
    .unwrap();
}

#[test]
fn mechanical_rename_fans_out_over_the_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());

    // Dry run: finds the dependent and its call site, changes nothing on disk.
    let out = Command::new(lex_bin())
        .args(["propagate", "--package", "up", "--rename", "old=new", "--workspace"])
        .arg(tmp.path())
        .output()
        .unwrap();
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "dry run: {so}{}", String::from_utf8_lossy(&out.stderr));
    assert!(so.contains("up.old → up.new"), "dry run names the edit: {so}");
    let dn_lib = tmp.path().join("dn/src/lib.lex");
    assert!(std::fs::read_to_string(&dn_lib).unwrap().contains("up.old(x)"), "dry run must not write");

    // Apply: rewrites the dependent, preserving the comment.
    let out = Command::new(lex_bin())
        .args(["propagate", "--package", "up", "--rename", "old=new", "--apply", "--workspace"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "apply: {}", String::from_utf8_lossy(&out.stderr));
    let after = std::fs::read_to_string(&dn_lib).unwrap();
    assert!(after.contains("up.new(x)"), "call site rewritten: {after}");
    assert!(!after.contains("up.old"), "no old refs left: {after}");
    assert!(after.contains("# uses the helper"), "comment preserved: {after}");
}

#[test]
fn semantic_migration_accepts_a_valid_regeneration_and_rejects_a_broken_one() {
    let tmp = tempfile::tempdir().unwrap();
    workspace(tmp.path());
    let dn_lib = tmp.path().join("dn/src/lib.lex");

    // A regenerator that emits a valid, self-contained module → the gate
    // (parse + type-check) accepts it, and --apply writes it.
    let good = "printf 'fn use_it(x :: Int) -> Int { x }\\n'";
    let out = Command::new(lex_bin())
        .args(["propagate", "--package", "up", "--symbol", "old", "--note", "old now returns 0 on negatives",
               "--regenerate-cmd", good, "--apply", "--workspace"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "valid regen must pass the gate: {}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read_to_string(&dn_lib).unwrap().contains("fn use_it(x :: Int) -> Int { x }"),
        "migrated source written");

    // Reset, then a regenerator that emits garbage → the gate rejects it and
    // the file is left unchanged.
    workspace(tmp.path());
    let bad = "printf 'this is not lex source'";
    let out = Command::new(lex_bin())
        .args(["propagate", "--package", "up", "--symbol", "old", "--note", "x",
               "--regenerate-cmd", bad, "--apply", "--workspace"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "broken regen must fail the gate");
    assert!(std::fs::read_to_string(&dn_lib).unwrap().contains("up.old(x)"),
        "rejected migration must not modify the file");
}
