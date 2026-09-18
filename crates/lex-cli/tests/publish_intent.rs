//! #131 / #839: `lex publish --intent-prompt ...` records an Intent and
//! stamps every emitted op with its id, so the op log carries *why* a
//! change happened. `lex recall --intent` and `lex op replay` read it
//! back. This is the ingestion seam an agent harness (lex-code) uses to
//! give the typed history real provenance.

use std::path::Path;
use std::process::Command;

fn lex() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lex"))
}

fn run_json(args: &[&str]) -> serde_json::Value {
    let out = lex().args(["--output", "json"]).args(args).output().expect("run lex");
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| {
        panic!("non-JSON from lex {args:?}: {e}\nstdout: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr))
    })
}

fn data(v: &serde_json::Value) -> &serde_json::Value {
    v.get("data").unwrap_or(v)
}

fn write(dir: &Path, name: &str, src: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, src).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn publish_with_intent_stamps_ops_and_is_recallable() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let store_s = store.to_string_lossy().into_owned();
    let file = write(tmp.path(), "m.lex", "fn triple(x :: Int) -> Int { x * 3 }\n");

    // Publish with a recorded intent.
    let v = run_json(&[
        "publish", &file, "--store", &store_s,
        "--intent-prompt", "add a triple function",
        "--intent-model", "ollama/qwen3.8:27b-mlx",
        "--intent-session", "run-42",
    ]);
    let d = data(&v);
    let intent_id = d["intent_id"].as_str().expect("publish output carries intent_id").to_string();
    assert!(!intent_id.is_empty());
    let op_id = d["ops"][0]["op_id"].as_str().expect("one op").to_string();

    // The op record carries the intent_id.
    let shown = run_json(&["op", "show", &op_id, "--store", &store_s]);
    let sd = data(&shown);
    let on_op = sd.get("intent_id").or_else(|| sd.get("op").and_then(|o| o.get("intent_id")));
    assert_eq!(on_op.and_then(|x| x.as_str()), Some(intent_id.as_str()),
        "op must be stamped with the intent: {shown}");

    // recall --intent finds it.
    let rec = run_json(&["recall", "--intent", &intent_id, "--store", &store_s]);
    let rd = data(&rec);
    assert_eq!(rd["count"].as_u64(), Some(1), "recall by intent: {rec}");
    assert_eq!(rd["ops"][0]["op_id"].as_str(), Some(op_id.as_str()));

    // op replay's request now carries the recorded prompt + model.
    let req = run_json(&["op", "replay", &op_id, "--store", &store_s]);
    let qd = data(&req);
    assert_eq!(qd["prompt"].as_str(), Some("add a triple function"));
    assert_eq!(qd["model"].as_str(), Some("ollama/qwen3.8:27b-mlx"));
    assert_eq!(qd["session_id"].as_str(), Some("run-42"));
}

#[test]
fn publish_without_intent_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let v = run_json(&["publish", &file, "--store", &store_s]);
    let d = data(&v);
    assert!(d["intent_id"].is_null(), "no intent recorded without --intent-prompt: {v}");
    assert_eq!(d["ops"].as_array().map(|a| a.len()), Some(1));
}

/// #949 phase 5: `--intent-issue` links the publish to the typed issue it
/// realizes. The recorded Intent carries `issue_id`, and its content-addressed
/// id differs from the same intent without the link.
#[test]
fn publish_with_intent_issue_links_the_intent_to_the_issue() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let store_s = store.to_string_lossy().into_owned();
    let file = write(tmp.path(), "m.lex", "fn triple(x :: Int) -> Int { x * 3 }\n");

    // A real issue in the same store, so the link points at something.
    let created = run_json(&[
        "issue", "create", "--store", &store_s,
        "--title", "add triple", "--shape", "typed_delta",
        "--api", "triple:(x :: Int) -> Int:added",
    ]);
    let issue_id = data(&created)["issue_id"].as_str().expect("issue id").to_string();

    let v = run_json(&[
        "publish", &file, "--store", &store_s,
        "--intent-prompt", "add a triple function",
        "--intent-session", "run-42",
        "--intent-issue", &issue_id,
    ]);
    let intent_id = data(&v)["intent_id"].as_str().expect("intent id").to_string();

    // The persisted Intent record carries the issue id.
    let raw = std::fs::read_to_string(store.join("intents").join(format!("{intent_id}.json")))
        .expect("intent record on disk");
    let intent: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(intent["issue_id"].as_str(), Some(issue_id.as_str()), "intent: {raw}");

    // Same prompt/session without the link is a different intent (the link
    // is part of the identity, so provenance can't be retro-fitted).
    let tmp2 = tempfile::tempdir().unwrap();
    let store2 = tmp2.path().join("store").to_string_lossy().into_owned();
    let file2 = write(tmp2.path(), "m.lex", "fn triple(x :: Int) -> Int { x * 3 }\n");
    let v2 = run_json(&[
        "publish", &file2, "--store", &store2,
        "--intent-prompt", "add a triple function",
        "--intent-session", "run-42",
    ]);
    assert_ne!(data(&v2)["intent_id"].as_str(), Some(intent_id.as_str()));
}

#[test]
fn intent_issue_without_prompt_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let out = lex().args(["publish", &file, "--store", &store_s, "--intent-issue", "abc"]).output().unwrap();
    assert!(!out.status.success(), "must refuse --intent-issue without --intent-prompt");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("require --intent-prompt"), "stderr: {err}");
}

#[test]
fn intent_model_or_session_without_prompt_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let out = lex().args(["publish", &file, "--store", &store_s, "--intent-model", "x/y"]).output().unwrap();
    assert!(!out.status.success(), "must refuse --intent-model without --intent-prompt");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("require --intent-prompt"), "stderr: {err}");
}
