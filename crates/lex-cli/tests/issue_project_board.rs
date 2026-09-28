//! A package is a project of typed issues with dependency edges. These pin
//! the three things a driver needs: the board (`list --project`), what can
//! start now (`next`), and a whole-project re-check that catches an earlier
//! issue silently regressing (`verify --project`).

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

fn create(store: &str, title: &str, api: &str, example: Option<&str>, dep: Option<&str>, project: &str) -> String {
    let mut args = vec!["issue", "create", "--title", title, "--shape", "typed_delta", "--api", api, "--project", project];
    if let Some(e) = example {
        args.extend(["--example", e]);
    }
    if let Some(d) = dep {
        args.extend(["--dep", d]);
    }
    data(&lex(store, &args))["issue_id"].as_str().unwrap().to_string()
}

fn publish(store: &str, dir: &std::path::Path, name: &str, src: &str) {
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    let out = lex(store, &["publish", path.to_str().unwrap(), "--activate"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn list_filters_by_project_and_reports_derived_state() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();
    let a = create(store, "one", "one:() -> Int", Some("one() => 1"), None, "pkg");
    let b = create(store, "two", "two:() -> Int", Some("two() => 2"), Some(&a), "pkg");
    create(store, "elsewhere", "z:() -> Int", None, None, "other");

    let v = data(&lex(store, &["issue", "list", "--project", "pkg"]));
    let issues = v["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 2, "other project must be excluded: {v}");
    let state = |id: &str| issues.iter().find(|i| i["issue_id"] == id).unwrap()["state"].clone();
    assert_eq!(state(&a), "open");
    assert_eq!(state(&b), "blocked");
    assert_eq!(v["counts"]["blocked"], 1);

    let only_open = data(&lex(store, &["issue", "list", "--project", "pkg", "--state", "open"]));
    assert_eq!(only_open["issues"].as_array().unwrap().len(), 1);
    // The count is over the project, not over the filtered view.
    assert_eq!(only_open["counts"]["total"], 2);
}

#[test]
fn next_offers_only_unblocked_issues_and_says_when_done() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();
    let a = create(store, "one", "one:() -> Int", Some("one() => 1"), None, "pkg");
    let b = create(store, "two", "two:() -> Int", Some("two() => 2"), Some(&a), "pkg");

    let v = data(&lex(store, &["issue", "next", "--project", "pkg"]));
    let ready: Vec<&str> = v["ready"].as_array().unwrap().iter().map(|i| i["issue_id"].as_str().unwrap()).collect();
    assert_eq!(ready, vec![a.as_str()], "b is blocked on a: {v}");
    assert_eq!(v["done"], false);

    publish(store, tmp.path(), "m.lex", "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n");
    assert!(lex(store, &["issue", "verify", &a]).status.success());
    // a verified, so b is now the only thing ready.
    let v = data(&lex(store, &["issue", "next", "--project", "pkg"]));
    assert_eq!(v["ready"][0]["issue_id"], b.as_str(), "{v}");

    assert!(lex(store, &["issue", "verify", &b]).status.success());
    let v = data(&lex(store, &["issue", "next", "--project", "pkg"]));
    assert_eq!(v["ready"].as_array().unwrap().len(), 0);
    assert_eq!(v["done"], true, "{v}");

    let limited = data(&lex(store, &["issue", "next", "--project", "pkg", "--limit", "0"]));
    assert_eq!(limited["ready"].as_array().unwrap().len(), 0);
}

#[test]
fn verify_project_orders_by_dependency_and_catches_a_regression() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();
    // Filed dependent-first on purpose: the order must come from the
    // edges, not from creation order alone.
    let a = create(store, "one", "one:() -> Int", Some("one() => 1"), None, "pkg");
    let b = create(store, "two", "two:() -> Int", Some("two() => 2"), Some(&a), "pkg");

    publish(store, tmp.path(), "m.lex", "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n");
    let out = lex(store, &["issue", "verify", "--project", "pkg"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v = data(&out);
    assert_eq!(v["verified"], 2, "{v}");
    assert_eq!(v["issues"][0]["issue_id"], a.as_str());

    // A later change drops `two` — closing some other issue would look
    // exactly like this. The project pass must fail, naming it.
    publish(store, tmp.path(), "m2.lex", "fn one() -> Int { 1 }\n");
    let out = lex(store, &["issue", "verify", "--project", "pkg"]);
    assert_eq!(out.status.code(), Some(1), "a regression must exit 1");
    let v = data(&out);
    assert_eq!(v["failed"], 1, "{v}");
    let failed = v["issues"].as_array().unwrap().iter().find(|i| i["verdict"] == "failed").unwrap();
    assert_eq!(failed["issue_id"], b.as_str());
}

#[test]
fn verify_rejects_an_id_together_with_a_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().to_str().unwrap();
    let a = create(store, "one", "one:() -> Int", None, None, "pkg");
    publish(store, tmp.path(), "m.lex", "fn one() -> Int { 1 }\n");
    let out = lex(store, &["issue", "verify", &a, "--project", "pkg"]);
    assert!(!out.status.success());
}
