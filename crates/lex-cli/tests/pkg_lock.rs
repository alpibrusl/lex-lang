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

/// A minimal gzipped-tar package archive (`lex.toml` + `src/lib.lex`), the
/// exact format `registry_ensure_cached` unpacks.
fn gz_tar_archive(name: &str) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut ar = tar::Builder::new(&mut enc);
        let toml = format!("[package]\nname = \"{name}\"\nversion = \"1.4.9\"\n");
        let add = |path: &str, data: &[u8], ar: &mut tar::Builder<&mut flate2::write::GzEncoder<Vec<u8>>>| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append_data(&mut h, path, data).unwrap();
        };
        add("lex.toml", toml.as_bytes(), &mut ar);
        add("src/lib.lex", b"fn helper(x :: Int) -> Int { x + 1 }\n", &mut ar);
        ar.finish().unwrap();
    }
    enc.finish().unwrap()
}

/// Spawn a registry serving both `/versions` (`body`) and a gz-tar archive at
/// `/v1/pkg/{name}/{version}/archive` for any version.
fn spawn_registry_with_archive(body: &'static str, archive: Vec<u8>) -> String {
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
            } else if url.ends_with("/archive") {
                let _ = req.respond(tiny_http::Response::from_data(archive.clone()));
            } else {
                // No /contract → verify treats it as unsigned (NoContract).
                let _ = req.respond(tiny_http::Response::empty(404));
            }
        }
    });
    format!("http://{addr}")
}

fn run_pkg_isolated_cache(dir: &Path, cache: &Path, sub: &str) -> std::process::Output {
    Command::new(lex_bin())
        .current_dir(dir)
        .env("LEX_PACKAGES_DIR", cache)
        .args(["pkg", sub])
        .output()
        .unwrap()
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
fn install_fetches_the_locked_version_for_a_constraint() {
    // The whole loop: a `^1.2` constraint has no single version to fetch, so
    // install must consult lex.lock. `lex pkg lock` pins 1.4.9; `lex pkg
    // install` then fetches exactly that release's archive.
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let registry = spawn_registry_with_archive(VERSIONS, gz_tar_archive("dep"));
    let app = tmp.path().join("app");
    write_consumer(&app, &registry, "^1.2");

    assert!(run_pkg_isolated_cache(&app, &cache, "lock").status.success());

    let out = run_pkg_isolated_cache(&app, &cache, "install");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "install should succeed via the lock: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Display shows the constraint resolving to the locked version.
    assert!(stdout.contains("1.4.9 (locked)"), "stdout={stdout}");
    // The exact locked release was extracted into the cache.
    assert!(cache.join("dep-1.4.9").join("src/lib.lex").exists(), "cache missing dep-1.4.9");
}

#[test]
fn install_without_a_lock_errors_for_a_constraint() {
    // A constraint dep with no lex.lock cannot be fetched — install must
    // refuse with a clear "run lex pkg lock", not fabricate a version.
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let registry = spawn_registry_with_archive(VERSIONS, gz_tar_archive("dep"));
    let app = tmp.path().join("app");
    write_consumer(&app, &registry, "^1.2");
    // Deliberately do NOT run `lex pkg lock`.

    let out = run_pkg_isolated_cache(&app, &cache, "install");
    assert!(!out.status.success(), "install must fail without a lock");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("lex pkg lock"), "output={combined}");
}

/// Spawn a registry that answers a versions listing ONLY on the public
/// route `/v1/public/<expect_seg>/…/versions`, 404 otherwise — so a test
/// fails unless the client actually targets the public surface (#917).
fn spawn_public_only_registry(expect_seg: &'static str, body: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!("expected IP listener"),
    };
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            let ok = url.contains(&format!("/v1/public/{expect_seg}/")) && url.contains("/versions");
            if ok {
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

#[test]
fn lock_targets_the_public_route_for_a_tenant_qualified_registry() {
    // registry = "<host>/acme" must resolve versions from
    // /v1/public/acme/dep/versions, NOT the auth'd /v1/pkg/dep/versions.
    let tmp = tempfile::tempdir().unwrap();
    let host = spawn_public_only_registry("acme", VERSIONS);
    let app = tmp.path().join("app");
    write_consumer(&app, &format!("{host}/acme"), "^1.2");

    let out = run_pkg(&app, "lock");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "lock must resolve via the public route: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lock = std::fs::read_to_string(app.join("lex.lock")).unwrap();
    assert!(lock.contains("version = \"1.4.9\""), "lock:\n{lock}");
}

#[test]
fn lock_passes_store_query_for_a_named_store_registry() {
    // registry = "<host>/acme/widgets" → /v1/public/acme/dep/versions?store=widgets
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!(),
    };
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            // Only answer when the store query is present on the public route.
            let ok = url.contains("/v1/public/acme/")
                && url.contains("/versions")
                && url.contains("store=widgets");
            if ok {
                let resp = tiny_http::Response::from_string(VERSIONS).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = req.respond(resp);
            } else {
                let _ = req.respond(tiny_http::Response::empty(404));
            }
        }
    });
    let host = format!("http://{addr}");
    let app = tmp.path().join("app");
    write_consumer(&app, &format!("{host}/acme/widgets"), "^1.2");

    let out = run_pkg(&app, "lock");
    assert!(
        out.status.success(),
        "lock must send ?store=widgets: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lock = std::fs::read_to_string(app.join("lex.lock")).unwrap();
    assert!(lock.contains("version = \"1.4.9\""), "lock:\n{lock}");
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
