//! #956 end to end through the CLI: a free-form issue, an agent's proposal,
//! a human's approval, and the gate judging the issue against the approved
//! acceptance under the issue's own id.

use std::process::Command;

fn lex(store: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lex"))
        .args(["--output", "json"])
        .args(args)
        .args(["--store", store])
        .output()
        .expect("run lex")
}

fn json(out: &std::process::Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!("non-JSON: {e}\nstdout: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    })
}

fn data(out: &std::process::Output) -> serde_json::Value {
    let v = json(out);
    v.get("data").cloned().unwrap_or(v)
}

#[test]
fn propose_approve_then_show_and_verify_use_the_approved_acceptance() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();

    let issue = data(&lex(store, &["issue", "create", "--title", "clamp", "--shape", "free_form"]));
    let id = issue["issue_id"].as_str().unwrap().to_string();

    let p = data(&lex(store, &[
        "issue", "propose", &id, "--shape", "failing_example",
        "--example", "clamp(9, 0, 3) => 3", "--by", "lex-code",
    ]));
    let pid = p["proposal_id"].as_str().unwrap().to_string();
    assert_eq!(p["status"], "pending");

    // Proposing is not approving: the issue is unchanged.
    let shown = json(&lex(store, &["issue", "show", &id]));
    assert!(shown.get("effective_acceptance").is_none());

    // An unsigned approval is refused.
    let unsigned = lex(store, &["issue", "approve", &pid]);
    assert!(!unsigned.status.success());

    let a = data(&lex(store, &["issue", "approve", &pid, "--by", "alfonso"]));
    assert_eq!(a["status"], "approved");
    assert_eq!(a["issue_id"], id.as_str());

    let shown = json(&lex(store, &["issue", "show", &id]));
    assert_eq!(shown["issue_id"], id.as_str(), "same issue, same id");
    assert_eq!(shown["acceptance"]["shape"], "free_form", "stored issue is never rewritten");
    assert_eq!(shown["effective_acceptance"]["shape"], "failing_example");
    assert_eq!(shown["approved_proposal"], pid.as_str());

    let listed = data(&lex(store, &["issue", "proposals", &id]));
    assert_eq!(listed["proposals"][0]["status"], "approved");

    // No head yet → verify refuses; with a head that lacks clamp, the
    // approved failing example makes the gate *fail* the issue — it is no
    // longer inconclusive.
    let src = tmp.path().join("m.lex");
    std::fs::write(&src, "fn one() -> Int { 1 }\n").unwrap();
    let publish = lex(store, &["publish", src.to_str().unwrap(), "--activate"]);
    assert!(publish.status.success(), "{}", String::from_utf8_lossy(&publish.stderr));
    let v = data(&lex(store, &["issue", "verify", &id]));
    assert_eq!(v["verdict"], "failed", "{v}");
}

#[test]
fn a_typed_issue_cannot_be_refined() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();
    let issue = data(&lex(store, &[
        "issue", "create", "--title", "t", "--shape", "failing_example", "--example", "one() => 1",
    ]));
    let id = issue["issue_id"].as_str().unwrap();
    let out = lex(store, &["issue", "propose", id, "--shape", "failing_example", "--example", "one() => 2"]);
    assert!(!out.status.success(), "proposing on a typed issue must be refused");
}
