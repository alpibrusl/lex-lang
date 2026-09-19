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
    run_json_env(args, &[])
}

/// Like [`run_json`] but sets env vars **on the child only**. Tests share one
/// process and run in parallel, so `std::env::set_var` in a test leaks into
/// every other test — which is exactly how this file first mis-reported a
/// synthesized session as shared. Pass env through the subprocess instead.
fn run_json_env(args: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let mut cmd = lex();
    cmd.args(["--output", "json"]).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run lex");
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

/// #970: a publish with no `--intent-prompt` still records an intent — an
/// explicitly *unattributed* one. Intent used to be opt-in, and the result was
/// 136k ops on the hosted store with ZERO intents: the "why was this changed"
/// provenance that distinguishes lex-vcs from git had no data on real history.
#[test]
fn publish_without_a_prompt_still_records_an_unattributed_intent() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let v = run_json(&["publish", &file, "--store", &store_s]);
    let d = data(&v);
    assert_eq!(d["ops"].as_array().map(|a| a.len()), Some(1));

    let intent_id = d["intent_id"]
        .as_str()
        .unwrap_or_else(|| panic!("an intent must be recorded even with no prompt: {v}"))
        .to_string();

    // It is queryable like any other intent — that is the whole point.
    let rec = run_json(&["recall", "--intent", &intent_id, "--store", &store_s]);
    let rd = data(&rec);
    assert_eq!(rd["count"].as_u64(), Some(1), "recall by the synthesized intent: {rec}");

    // And it is honest: the prompt is an unmistakable marker, never a
    // plausible-looking invented one, and the producer is `cli/unknown`.
    let req = run_json(&["op", "replay", rd["ops"][0]["op_id"].as_str().unwrap(), "--store", &store_s]);
    let qd = data(&req);
    let prompt = qd["prompt"].as_str().unwrap_or_default();
    assert!(
        prompt.contains("unattributed"),
        "the synthesized prompt must mark itself unattributed, got {prompt:?}"
    );
    assert_eq!(qd["model"].as_str(), Some("cli/unknown"));
}

/// The synthesized session must not be a constant: that would collapse every
/// publish ever made into one bogus "session" and make `recall --session`
/// useless exactly where it should help.
#[test]
fn synthesized_sessions_differ_between_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a").to_string_lossy().into_owned();
    let b = tmp.path().join("b").to_string_lossy().into_owned();
    let f1 = write(tmp.path(), "s1.lex", "fn one(x :: Int) -> Int { x + 1 }\n");
    let f2 = write(tmp.path(), "s2.lex", "fn two(x :: Int) -> Int { x + 2 }\n");

    let s1 = session_of(&a, &f1);
    let s2 = session_of(&b, &f2);
    assert_ne!(s1, s2, "two separate invocations must not share a session id");
    assert!(s1.starts_with("cli-"), "unexpected synthesized session: {s1}");
}

/// `LEX_INTENT_SESSION` lets a harness give several invocations one logical run.
#[test]
fn lex_intent_session_env_supplies_continuity() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a").to_string_lossy().into_owned();
    let b = tmp.path().join("b").to_string_lossy().into_owned();
    let f1 = write(tmp.path(), "e1.lex", "fn one(x :: Int) -> Int { x + 1 }\n");
    let f2 = write(tmp.path(), "e2.lex", "fn two(x :: Int) -> Int { x + 2 }\n");

    let env = [("LEX_INTENT_SESSION", "harness-run-7")];
    let s1 = session_of_env(&a, &f1, &env);
    let s2 = session_of_env(&b, &f2, &env);

    assert_eq!(s1, "harness-run-7");
    assert_eq!(s2, "harness-run-7", "the env session must carry across invocations");
}

/// Publish `file` into `store` with no intent flags, and report the session id
/// the synthesized intent recorded.
fn session_of(store: &str, file: &str) -> String {
    session_of_env(store, file, &[])
}

fn session_of_env(store: &str, file: &str, env: &[(&str, &str)]) -> String {
    let v = run_json_env(&["publish", file, "--store", store], env);
    let d = data(&v);
    let op = d["ops"][0]["op_id"].as_str().expect("op id").to_string();
    let req = run_json_env(&["op", "replay", &op, "--store", store], env);
    data(&req)["session_id"].as_str().unwrap_or_default().to_string()
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

/// #970 behaviour change: `--intent-issue` no longer requires
/// `--intent-prompt`. Binding an op to a typed issue without prose is useful,
/// and refusing it only pushed callers toward recording nothing at all.
#[test]
fn intent_issue_without_prompt_is_accepted_and_linked() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let v = run_json(&["publish", &file, "--store", &store_s, "--intent-issue", "abc"]);
    let d = data(&v);
    let intent_id = d["intent_id"]
        .as_str()
        .unwrap_or_else(|| panic!("--intent-issue alone must still record an intent: {v}"))
        .to_string();
    // The issue link changes the content-addressed id, so it is genuinely
    // recorded rather than dropped.
    let plain = {
        let other = tmp.path().join("other").to_string_lossy().into_owned();
        let f2 = write(tmp.path(), "q2.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
        let v2 = run_json(&["publish", &f2, "--store", &other]);
        data(&v2)["intent_id"].as_str().unwrap_or_default().to_string()
    };
    assert_ne!(intent_id, plain, "the issue link must affect the intent id");
}

/// Same for `--intent-model` / `--intent-session`: honoured on their own.
#[test]
fn intent_model_and_session_without_prompt_are_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store").to_string_lossy().into_owned();
    let file = write(tmp.path(), "q.lex", "fn quad(x :: Int) -> Int { x * 4 }\n");
    let v = run_json(&[
        "publish", &file, "--store", &store_s,
        "--intent-model", "x/y", "--intent-session", "run-9",
    ]);
    let d = data(&v);
    let op = d["ops"][0]["op_id"].as_str().expect("op id").to_string();
    let req = run_json(&["op", "replay", &op, "--store", &store_s]);
    let qd = data(&req);
    assert_eq!(qd["model"].as_str(), Some("x/y"), "declared model must be recorded: {req}");
    assert_eq!(qd["session_id"].as_str(), Some("run-9"));
    // The prompt is still the unattributed marker — those flags don't invent one.
    assert!(qd["prompt"].as_str().unwrap_or_default().contains("unattributed"));
}
