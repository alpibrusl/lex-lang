//! `lex issue verify` must not pass an issue whose declared `--dep` hasn't
//! itself verified. The derived board state (#949 phase 3, `issue_status`)
//! already computes this as `Blocked`; this is the same check applied where
//! a script actually branches on the verdict, not just where a board reads
//! it.

use std::process::Command;

fn lex(store: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lex"))
        .args(["--output", "json"])
        .args(args)
        .args(["--store", store])
        .output()
        .expect("run lex")
}

fn data(out: &std::process::Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!("non-JSON: {e}\nstdout: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    });
    v.get("data").cloned().unwrap_or(v)
}

#[test]
fn verify_refuses_to_pass_an_issue_blocked_on_an_unverified_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();

    let a = data(&lex(store, &[
        "issue", "create", "--title", "add one", "--shape", "typed_delta",
        "--api", "one:() -> Int:added",
    ]));
    let a_id = a["issue_id"].as_str().unwrap().to_string();

    let b = data(&lex(store, &[
        "issue", "create", "--title", "add two", "--shape", "typed_delta",
        "--api", "two:() -> Int:added", "--dep", &a_id,
    ]));
    let b_id = b["issue_id"].as_str().unwrap().to_string();

    // Both `one` and `two` already exist at head — so if the dependency gate
    // did nothing, B's own oracle would pass on its own merits. That's the
    // point: this only proves the gate ran if B comes back blocked anyway.
    let src = tmp.path().join("m.lex");
    std::fs::write(&src, "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n").unwrap();
    let publish = lex(store, &["publish", src.to_str().unwrap(), "--activate"]);
    assert!(publish.status.success(), "{}", String::from_utf8_lossy(&publish.stderr));

    // A is unverified — B must come back blocked, not verified, even though
    // `two` genuinely exists with the declared signature.
    let v = data(&lex(store, &["issue", "verify", &b_id]));
    assert_eq!(v["verdict"], "inconclusive", "{v}");
    assert!(
        v["detail"].as_str().unwrap().contains(&a_id),
        "detail should name the unverified dependency: {v}"
    );

    // Verify A for real.
    let va = data(&lex(store, &["issue", "verify", &a_id]));
    assert_eq!(va["verdict"], "verified", "{va}");

    // Now B is unblocked and its own (real) oracle holds.
    let v2 = data(&lex(store, &["issue", "verify", &b_id]));
    assert_eq!(v2["verdict"], "verified", "{v2}");
}

#[test]
fn verify_still_reports_unmet_deps_even_when_the_dep_id_never_existed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();

    let bogus = "f".repeat(64);
    let b = data(&lex(store, &[
        "issue", "create", "--title", "add two", "--shape", "typed_delta",
        "--api", "two:() -> Int:added", "--dep", &bogus,
    ]));
    let b_id = b["issue_id"].as_str().unwrap().to_string();

    let src = tmp.path().join("m.lex");
    std::fs::write(&src, "fn two() -> Int { 2 }\n").unwrap();
    let publish = lex(store, &["publish", src.to_str().unwrap(), "--activate"]);
    assert!(publish.status.success(), "{}", String::from_utf8_lossy(&publish.stderr));

    // An unknown dependency counts as blocking, conservatively — same
    // contract `issue_status` documents for the board view.
    let v = data(&lex(store, &["issue", "verify", &b_id]));
    assert_eq!(v["verdict"], "inconclusive", "{v}");
    assert!(v["detail"].as_str().unwrap().contains(&bogus), "{v}");
}
