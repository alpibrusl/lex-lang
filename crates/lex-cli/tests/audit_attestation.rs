//! `lex audit --effect K --store DIR` — EffectAudit attestation
//! producer (#132 last producer slice).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn write_src(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

fn list_all_attestations(store: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "attest", "filter",
            "--store", store.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "attest filter: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn audit_with_store_emits_passed_for_pure_fns() {
    let store = tempdir().unwrap();
    let src = write_src(store.path(), "a.lex", "fn pure_fn(n :: Int) -> Int { n + 1 }\n");

    let out = Command::new(lex_bin())
        .args([
            "audit", "--effect", "io",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "audit: {}", String::from_utf8_lossy(&out.stderr));

    let v = list_all_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let effect_atts: Vec<&serde_json::Value> = atts.iter()
        .filter(|a| a["kind"]["kind"] == "effect_audit")
        .collect();
    assert_eq!(effect_atts.len(), 1, "expected one EffectAudit attestation");
    assert_eq!(effect_atts[0]["result"]["result"], "passed");
    assert_eq!(effect_atts[0]["produced_by"]["tool"], "lex audit");
}

#[test]
fn audit_with_store_emits_failed_for_fn_touching_forbidden_effect() {
    let store = tempdir().unwrap();
    let src = write_src(store.path(), "b.lex",
        "import \"std.io\" as io\nfn say(line :: Str) -> [io] Nil { io.print(line) }\n");

    let out = Command::new(lex_bin())
        .args([
            "audit", "--effect", "io",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "audit: {}", String::from_utf8_lossy(&out.stderr));

    let v = list_all_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let effect_atts: Vec<&serde_json::Value> = atts.iter()
        .filter(|a| a["kind"]["kind"] == "effect_audit")
        .collect();
    assert_eq!(effect_atts.len(), 1, "expected one EffectAudit attestation");
    assert_eq!(effect_atts[0]["result"]["result"], "failed");
    let detail = effect_atts[0]["result"]["detail"].as_str().unwrap_or("");
    assert!(detail.contains("io"), "detail should mention io: {detail}");
}

#[test]
fn audit_emits_one_attestation_per_fn_in_mixed_file() {
    // One pure fn + one fn touching io. Each gets its own
    // attestation: passed and failed respectively.
    let store = tempdir().unwrap();
    let src = write_src(store.path(), "mixed.lex",
        "import \"std.io\" as io\n\
         fn pure_fn(n :: Int) -> Int { n + 1 }\n\
         fn impure_fn(line :: Str) -> [io] Nil { io.print(line) }\n");

    let out = Command::new(lex_bin())
        .args([
            "audit", "--effect", "io",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success());

    let v = list_all_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let effect_atts: Vec<&serde_json::Value> = atts.iter()
        .filter(|a| a["kind"]["kind"] == "effect_audit")
        .collect();
    assert_eq!(effect_atts.len(), 2);
    let passed_count = effect_atts.iter()
        .filter(|a| a["result"]["result"] == "passed").count();
    let failed_count = effect_atts.iter()
        .filter(|a| a["result"]["result"] == "failed").count();
    assert_eq!(passed_count, 1);
    assert_eq!(failed_count, 1);
}

#[test]
fn audit_store_without_effect_errors() {
    // `--store` without `--effect` makes no claim — refuse to write
    // a vacuous attestation.
    let store = tempdir().unwrap();
    let src = write_src(store.path(), "a.lex", "fn id(n :: Int) -> Int { n }\n");

    let out = Command::new(lex_bin())
        .args([
            "audit",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success(), "expected error when --store without --effect");
}

#[test]
fn audit_without_store_writes_no_attestations() {
    // Backwards-compat: existing audit invocations stay clean.
    let store = tempdir().unwrap();
    let src = write_src(store.path(), "a.lex", "fn id(n :: Int) -> Int { n }\n");

    let out = Command::new(lex_bin())
        .args([
            "audit", "--effect", "io",
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success());
    assert!(!store.path().join("attestations").exists());
}
