//! `lex publish` over the op-DAG model.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

#[test]
fn publish_creates_main_branch_with_head_op() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run publish");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert!(ops.is_array());
    assert!(!ops.as_array().unwrap().is_empty(), "expected at least one op");
    assert!(store.path().join("branches/main.json").exists(),
        "main branch file should exist post-publish");
}

#[test]
fn republish_unchanged_source_emits_zero_ops() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    let out = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert_eq!(ops.as_array().unwrap().len(), 0, "expected 0 ops on no-op republish");
}

#[test]
fn blame_with_evidence_attaches_typecheck_attestation() {
    // After `lex publish`, every accepted op carries a TypeCheck::Passed
    // attestation (#132 + #147). `lex blame --with-evidence` must
    // surface that evidence under the corresponding history entry.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            "--with-evidence",
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run blame");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "fac").expect("fac in blame");
    let history = entry["history"].as_array().expect("history array");
    assert!(!history.is_empty(), "expected non-empty history");
    let stage_entry = &history[0];
    let atts = stage_entry["attestations"].as_array()
        .expect("attestations field present under --with-evidence");
    assert!(!atts.is_empty(), "expected at least one TypeCheck attestation");
    assert_eq!(atts[0]["kind"]["kind"], "type_check");
    assert_eq!(atts[0]["result"]["result"], "passed");
    assert_eq!(atts[0]["produced_by"]["tool"], "lex-store");
}

#[test]
fn blame_without_evidence_does_not_attach_attestations() {
    // Without --with-evidence the JSON shape stays unchanged
    // (no `attestations` field). Important for backward
    // compatibility with consumers that didn't ask for evidence.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run blame");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "fac").unwrap();
    let history = entry["history"].as_array().unwrap();
    assert!(
        history[0].get("attestations").is_none(),
        "attestations field must not appear without --with-evidence",
    );
}

#[test]
fn blame_after_rename_shows_one_causal_event() {
    let store = tempdir().unwrap();
    let src1 = store.path().join("a.lex");
    std::fs::write(&src1, "fn parse(s :: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    // Rename: same body, new name.
    std::fs::write(&src1, "fn parse_int(s :: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args(["--output","json","blame","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").or_else(|| v.get("blame")).unwrap();
    let parse_int = blame.as_array().unwrap().iter()
        .find(|e| e["name"] == "parse_int").expect("parse_int in blame");
    let causal = parse_int["causal_history"].as_array().expect("causal_history");
    let renames: Vec<_> = causal.iter()
        .filter(|e| e["kind"] == "rename_symbol").collect();
    assert_eq!(renames.len(), 1, "expected exactly one rename in causal history");
}

fn publish_dir(root: &std::path::Path, store: &std::path::Path) -> std::process::Output {
    Command::new(lex_bin())
        .args([
            "--output", "json", "publish", "--activate",
            "--store", store.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap()
}

fn ops_len(out: &std::process::Output) -> usize {
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v.pointer("/data/ops")
        .or_else(|| v.get("ops"))
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or_else(|| panic!("no ops field; stderr: {}", String::from_utf8_lossy(&out.stderr)))
}

/// `lex publish <dir>` publishes a whole multi-module package as one
/// prefix-mangled program (#894), and a byte-identical republish is a
/// no-op. The two modules share a *structurally identical* `dup` helper:
/// same signature and body, different files, so they get one
/// name-independent `StageId` but distinct `SigId`s. Reading the old side
/// by `StageId` dropped one of the pair and re-added it on every
/// republish (unbounded op growth, #818/#826/#894) — this pins that shut.
#[test]
fn publish_package_is_idempotent_across_shared_stage_ids() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n").unwrap();
    let src = root.join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.lex"), "fn dup(x :: Int) -> Int { x }\n").unwrap();
    std::fs::write(src.join("b.lex"), "fn dup(x :: Int) -> Int { x }\n").unwrap();
    std::fs::write(
        src.join("main.lex"),
        "import \"./a\" as a\nimport \"./b\" as b\nfn run(n :: Int) -> Int { a.dup(n) + b.dup(n) }\n",
    )
    .unwrap();
    let store = root.join(".lex/store");

    let p1 = publish_dir(root, &store);
    assert!(p1.status.success(), "publish #1: {}", String::from_utf8_lossy(&p1.stderr));
    // Both `dup`s + `run` — the sibling-name collision that made
    // module-by-module publishing fail can't happen (mangled names).
    assert!(ops_len(&p1) >= 3, "expected >=3 ops (both dups + run), got {}", ops_len(&p1));

    let p2 = publish_dir(root, &store);
    assert!(p2.status.success(), "publish #2: {}", String::from_utf8_lossy(&p2.stderr));
    assert_eq!(ops_len(&p2), 0, "a package republish must be a no-op (idempotent)");
}

/// #894 slice 2: a package publish records each declaration's source
/// file on its `AddFunction`/`AddType` op (so `export-git` can later
/// de-flatten the package). A single-file publish records none, keeping
/// those ops byte-identical (OpId-stable).
#[test]
fn package_publish_records_in_file_per_declaration() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lex.toml"), "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n").unwrap();
    let src = root.join("src");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("a.lex"), "type Wid = { n :: Int }\nfn helper(x :: Int) -> Int { x + 1 }\n").unwrap();
    std::fs::write(src.join("main.lex"), "import \"./a\" as a\nfn run(n :: Int) -> Int { a.helper(n) }\n").unwrap();
    let store = root.join(".lex/store");

    let out = Command::new(lex_bin())
        .args([
            "--output", "json", "publish", "--activate",
            "--store", store.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).unwrap().as_array().unwrap();

    // Every add_function / add_type op carries an in_file naming a src file.
    let adds: Vec<&serde_json::Value> = ops
        .iter()
        .filter(|o| matches!(o.pointer("/kind/op").and_then(|s| s.as_str()), Some("add_function") | Some("add_type")))
        .collect();
    assert!(adds.len() >= 3, "expected >=3 add ops (Wid, helper, run), got {}", adds.len());
    for a in &adds {
        let in_file = a.pointer("/kind/in_file").and_then(|s| s.as_str());
        assert!(
            matches!(in_file, Some(f) if f.starts_with("src/") && f.ends_with(".lex")),
            "add op missing a src/ in_file: {a}"
        );
    }
    // `helper` and `Wid` are in a.lex; `run` is in main.lex.
    let files: std::collections::BTreeSet<&str> =
        adds.iter().filter_map(|a| a.pointer("/kind/in_file").and_then(|s| s.as_str())).collect();
    assert!(files.contains("src/a.lex") && files.contains("src/main.lex"),
        "expected both src/a.lex and src/main.lex, got {files:?}");
}
