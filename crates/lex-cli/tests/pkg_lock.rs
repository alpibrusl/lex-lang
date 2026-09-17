//! End-to-end for `lex pkg lock` / `lex pkg update` (#893): resolve a
//! registry dependency's semver constraint against the registry's published
//! release set (`GET /v1/pkg/{name}/versions`, #911) and write a `lex.lock`
//! pinning the exact version + op-log head.
//!
//! A `tiny_http` mock registry serves a versions listing; the CLI is driven as
//! a real subprocess against it, so the HTTP fetch → `best_match` resolve →
//! lockfile-write path is covered without a live hub.

use std::path::Path;
use std::process::Command;
use std::thread;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

/// Spawn a mock registry whose `/v1/pkg/{name}/versions` returns `body`.
/// Returns its base URL.
fn spawn_registry(body: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!("expected IP listener"),
    };
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            if url.ends_with("/versions") {
                let resp = tiny_http::Response::from_string(body).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = req.respond(resp);
            } else {
                let _ = req.respond(tiny_http::Response::empty(404));
            }
        }
    });
    format!("http://{addr}")
}

/// Write a consumer project depending on `dep` at `constraint` from `registry`.
fn write_consumer(dir: &Path, registry: &str, constraint: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("lex.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\ndep = {{ registry = \"{registry}\", version = \"{constraint}\" }}\n"
        ),
    )
    .unwrap();
}

fn run_pkg(dir: &Path, sub: &str) -> std::process::Output {
    Command::new(lex_bin())
        .current_dir(dir)
        .args(["pkg", sub])
        .output()
        .unwrap()
}

const VERSIONS: &str = r#"{
  "name": "dep",
  "latest": "2.0.0",
  "versions": [
    {"version": "1.0.0", "head_op": "op_v100"},
    {"version": "1.2.0", "head_op": "op_v120"},
    {"version": "1.4.9", "head_op": "op_v149"},
    {"version": "2.0.0", "head_op": "op_v200"}
  ]
}"#;

#[test]
fn lock_pins_the_highest_matching_release() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = spawn_registry(VERSIONS);
    let app = tmp.path().join("app");
    write_consumer(&app, &registry, "^1.2");

    let out = run_pkg(&app, "lock");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "lock should succeed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lock = std::fs::read_to_string(app.join("lex.lock")).expect("lex.lock written");
    // ^1.2 over {1.0,1.2,1.4.9,2.0} resolves to 1.4.9, pinning its head.
    assert!(lock.contains("version = \"1.4.9\""), "lock:\n{lock}");
    assert!(lock.contains("op_v149"), "lock:\n{lock}");
    assert!(lock.contains("constraint = \"^1.2\""), "lock:\n{lock}");
    // 2.0.0 is outside the caret range and must not appear as the pin.
    assert!(!lock.contains("version = \"2.0.0\""), "lock:\n{lock}");
}

#[test]
fn lock_keeps_a_valid_pin_but_update_advances_it() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");

    // First registry only offers up to 1.2.0.
    let reg1 = spawn_registry(
        r#"{"versions":[{"version":"1.0.0","head_op":"op_a"},{"version":"1.2.0","head_op":"op_b"}]}"#,
    );
    write_consumer(&app, &reg1, "^1.0");
    assert!(run_pkg(&app, "lock").status.success());
    let first = std::fs::read_to_string(app.join("lex.lock")).unwrap();
    assert!(first.contains("version = \"1.2.0\""), "first lock:\n{first}");

    // Now point the same constraint at a registry that also offers 1.9.0, but
    // rewrite lex.toml to keep the existing lock's registry URL so the pin is
    // recognised as "still offered".
    let reg2 = spawn_registry(
        r#"{"versions":[{"version":"1.2.0","head_op":"op_b"},{"version":"1.9.0","head_op":"op_c"}]}"#,
    );
    write_consumer(&app, &reg2, "^1.0");
    // Rewrite the lockfile's registry to reg2 so keep-existing can match it.
    let relocked = first.replace(&reg1, &reg2);
    std::fs::write(app.join("lex.lock"), relocked).unwrap();

    // `lock` keeps 1.2.0 (still satisfies ^1.0 and is still offered).
    assert!(run_pkg(&app, "lock").status.success());
    let kept = std::fs::read_to_string(app.join("lex.lock")).unwrap();
    assert!(kept.contains("version = \"1.2.0\""), "kept lock:\n{kept}");

    // `update` advances to the highest match, 1.9.0.
    assert!(run_pkg(&app, "update").status.success());
    let updated = std::fs::read_to_string(app.join("lex.lock")).unwrap();
    assert!(updated.contains("version = \"1.9.0\""), "updated lock:\n{updated}");
    assert!(updated.contains("op_c"), "updated lock:\n{updated}");
}

#[test]
fn lock_reports_when_nothing_satisfies_the_constraint() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = spawn_registry(VERSIONS);
    let app = tmp.path().join("app");
    write_consumer(&app, &registry, "^3"); // no 3.x offered

    let out = run_pkg(&app, "lock");
    assert!(!out.status.success(), "lock should fail when unresolvable");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no released version satisfies"),
        "stderr={stderr}"
    );
    assert!(!app.join("lex.lock").exists(), "no lockfile on failed resolve");
}
