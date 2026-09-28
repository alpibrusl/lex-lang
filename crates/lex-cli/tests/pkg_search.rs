//! `lex pkg search` end to end against a fake GitHub: the search API and the
//! raw `lex.toml` host are one local server, wired in through the
//! `LEX_PKG_SEARCH_API` / `LEX_PKG_RAW_BASE` overrides.
//!
//! What this pins: a task-shaped query finds a package whose description is
//! empty (the `lex-web` case that motivated the command), repos without a
//! `lex.toml` are dropped, the printed dependency line is one `lex.toml`
//! accepts, a multi-word query that ANDs to nothing is retried as "any word",
//! and an unreachable backend is a clear error, not a silent "no results".

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

/// The queries the server saw, so a test can assert on what was searched.
type Seen = Arc<Mutex<Vec<String>>>;

fn repo(name: &str, description: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "full_name": format!("acme/{name}"),
        "html_url": format!("https://github.com/acme/{name}"),
        "description": description,
        "default_branch": "main",
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /search/repositories?q=…` answers from `search`, keyed on whether the
/// query used ` OR `; `GET /acme/<repo>/main/lex.toml` answers from `manifests`.
fn start(
    and_hits: Vec<serde_json::Value>,
    or_hits: Vec<serde_json::Value>,
    manifests: Vec<(&'static str, &'static str)>,
) -> (String, Seen) {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let seen: Seen = Arc::default();
    let seen2 = seen.clone();
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            let (status, body) = if let Some(q) = url.strip_prefix("/search/repositories?q=") {
                let q = percent_decode(q.split('&').next().unwrap_or(""));
                seen2.lock().unwrap().push(q.clone());
                let items = if q.contains(" OR ") { &or_hits } else { &and_hits };
                (200, serde_json::json!({ "items": items }).to_string())
            } else if let Some(path) = url.strip_prefix("/acme/") {
                let name = path.split('/').next().unwrap_or("");
                match manifests.iter().find(|(n, _)| *n == name) {
                    Some((_, toml)) => (200, toml.to_string()),
                    None => (404, "404: Not Found".to_string()),
                }
            } else {
                (404, String::new())
            };
            let _ = req.respond(tiny_http::Response::from_string(body).with_status_code(status));
        }
    });
    (format!("http://{addr}"), seen)
}

fn search(base: &str, args: &[&str]) -> std::process::Output {
    Command::new(lex_bin())
        .args(["pkg", "search"])
        .args(args)
        .env("LEX_PKG_SEARCH_API", base)
        .env("LEX_PKG_RAW_BASE", base)
        .env_remove("GITHUB_TOKEN")
        .output()
        .unwrap()
}

const WEB_TOML: &str = "[package]\nname = \"acme-web\"\nversion = \"0.3.0\"\nlex = \"0.11.32\"\n";
const CRYPTO_TOML: &str =
    "[package]\nname = \"acme-crypto\"\nversion = \"1.2.0\"\ndescription = \"JWT and hashing\"\n";

#[test]
fn finds_a_package_with_no_description_and_prints_the_toml_line() {
    let (base, seen) = start(
        // README-only hits: no description on GitHub, none in lex.toml.
        vec![repo("acme-web", None), repo("website", Some("marketing site")), repo("acme-crypto", None)],
        vec![],
        vec![("acme-web", WEB_TOML), ("acme-crypto", CRYPTO_TOML)],
    );
    let out = search(&base, &["router"]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}{}", String::from_utf8_lossy(&out.stderr));

    assert!(so.contains("acme-web 0.3.0"), "name + version from lex.toml: {so}");
    assert!(so.contains(r#"acme-web = { git = "https://github.com/acme/acme-web" }"#), "toml line: {so}");
    assert!(so.contains("lex pkg add acme-web --git https://github.com/acme/acme-web"), "{so}");
    assert!(so.contains("needs lex >= 0.11.32"), "{so}");
    assert!(so.contains("JWT and hashing"), "description comes from lex.toml: {so}");
    assert!(!so.contains("website"), "a repo with no lex.toml is not a package: {so}");

    let q = seen.lock().unwrap().clone();
    assert_eq!(q.len(), 1, "one AND query is enough when it hits: {q:?}");
    assert!(q[0].contains("org:alpibrusl"), "scoped to the package org by default: {q:?}");
}

#[test]
fn the_printed_dependency_line_is_valid_lex_toml() {
    let (base, _) = start(vec![repo("acme-web", None)], vec![], vec![("acme-web", WEB_TOML)]);
    let out = search(&base, &["router", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let line = v["results"][0]["toml_line"].as_str().unwrap();

    // Paste it into a project the way an agent would and let the real
    // manifest reader (`lex pkg list`) accept it.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("lex.toml"), format!("[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n[dependencies]\n{line}\n")).unwrap();
    let list = Command::new(lex_bin()).args(["pkg", "list"]).current_dir(dir.path()).output().unwrap();
    let so = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success() && so.contains("acme-web") && so.contains("https://github.com/acme/acme-web"), "{so}");
}

#[test]
fn multi_word_query_that_ands_to_nothing_is_retried_as_any_word() {
    let (base, seen) = start(vec![], vec![repo("acme-web", None)], vec![("acme-web", WEB_TOML)]);
    let out = search(&base, &["REST", "API", "todo", "server"]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    assert!(so.contains("acme-web") && so.contains("matching any word"), "{so}");
    let q = seen.lock().unwrap().clone();
    assert_eq!(q.len(), 2, "AND then OR: {q:?}");
    assert!(q[1].contains("rest OR api OR todo OR server"), "{q:?}");
}

#[test]
fn no_match_says_nothing_to_reuse_and_exits_zero() {
    let (base, _) = start(vec![], vec![], vec![]);
    let out = search(&base, &["zzzz"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to reuse"));
}

#[test]
fn a_query_cannot_widen_the_search_beyond_the_org() {
    let (base, seen) = start(vec![], vec![], vec![]);
    let out = search(&base, &["router", "org:someone-else", "user:x"]);
    assert!(out.status.success());
    let q = seen.lock().unwrap().clone();
    assert_eq!(q[0].matches("org:").count(), 1, "only our own org: qualifier: {q:?}");
    assert!(!q[0].contains("user:"), "{q:?}");
}

#[test]
fn unreachable_backend_is_an_error_with_a_browse_fallback() {
    // A port nothing listens on.
    let out = search("http://127.0.0.1:9", &["router"]);
    assert!(!out.status.success(), "a dead backend must not look like 'no results'");
    let se = String::from_utf8_lossy(&out.stderr);
    assert!(se.contains("package search unavailable") && se.contains("browse instead"), "{se}");
}

#[test]
fn empty_query_prints_usage() {
    let (base, _) = start(vec![], vec![], vec![]);
    let out = search(&base, &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: lex pkg search"));
}

const WEB_DESC_TOML: &str = "[package]\nname = \"acme-web\"\nversion = \"0.3.0\"\ndescription = \"HTTP REST framework with a typed router and path params\"\n";

#[test]
fn a_thin_strict_search_is_topped_up_and_the_best_match_ranks_first() {
    // The regression this pins: a long natural-language query ANDed down to
    // two accidental README matches, so the any-word fallback never ran and
    // the package that fits (matches on name/description) was never shown.
    let (base, seen) = start(
        vec![repo("acme-noise", None)],
        vec![repo("acme-noise", None), repo("acme-web", None)],
        vec![("acme-noise", "[package]\nname = \"acme-noise\"\n"), ("acme-web", WEB_DESC_TOML)],
    );
    let out = search(&base, &["http", "router", "rest", "api", "server", "with", "path", "params"]);
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{so}");
    let web = so.find("acme-web").expect("the fitting package must be shown");
    let noise = so.find("acme-noise").expect("the accidental match is still listed");
    assert!(web < noise, "description matches outrank a README-only hit: {so}");
    assert_eq!(seen.lock().unwrap().len(), 2, "AND then OR");
    assert!(!seen.lock().unwrap()[1].contains(" with "), "stopwords never reach GitHub: {:?}", seen.lock().unwrap());
}
