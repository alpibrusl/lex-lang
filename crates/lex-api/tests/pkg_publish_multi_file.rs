//! `pkg_publish_handler` used to rebuild its "old_fns" snapshot (the
//! branch's current function set, needed for rename/modify detection)
//! from scratch on every file in an uploaded package, via an
//! O(N)-per-call `branch_head` walk plus a disk fetch per live
//! function -- see alpibrusl/lex-lang#813. On a real tenant with
//! 110k+ accumulated ops this hung a production `lex-hub` server for
//! tens of minutes. The fix computes the snapshot once and updates it
//! in memory using each file's own already-computed diff report
//! afterward.
//!
//! This proves the in-memory tracking stays exactly equivalent to a
//! fresh re-fetch: publish a package where two files in the SAME
//! request both redefine the same function to the SAME new body. The
//! first file's diff must see it as changed (one `modify_body` op);
//! the second file's diff, if it correctly sees the first file's
//! just-published update, sees no further change (same body both
//! sides) and emits nothing more for it. A stale snapshot would make
//! the second file re-diff against the pre-request body -- which, as
//! verified while developing this test, doesn't just produce a
//! spurious extra op: `store.publish_program` itself rejects the
//! resulting inconsistent diff outright ("diff mentions
//! removed/modified name `..` but old_name_to_sig has no entry"), so
//! this is a correctness fix, not only a performance one.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
    _server_holder: Arc<()>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || {
        lex_api::serve_on(server, state);
    });
    wait_until_serving(&addr);
    (Server { addr, _join: Some(join), _server_holder: Arc::new(()) }, tmp)
}

fn wait_until_serving(addr: &SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
}

/// POST a raw binary body (as `/v1/pkg/publish` expects — a tar.gz
/// archive, not JSON) and return (status, body).
fn post_bytes(addr: &SocketAddr, path: &str, body: &[u8]) -> (u16, String) {
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ).into_bytes();
    req.extend_from_slice(body);

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match try_post(addr, &req) {
            Ok(result) => return result,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    panic!("POST {path} failed after retries: {e}");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn try_post(addr: &SocketAddr, req: &[u8]) -> Result<(u16, String), String> {
    let mut s = TcpStream::connect_timeout(addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e| e.to_string())?;
    s.write_all(req).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Err("empty response".into());
    }
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    Ok((status, body.to_string()))
}

/// Build a `.tar.gz` containing `lex.toml` + one or more `src/*.lex`
/// files, the shape `POST /v1/pkg/publish` expects.
fn pkg_archive(name: &str, version: &str, src_files: &[(&str, &str)]) -> Vec<u8> {
    let toml = format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n");
    let mut files: Vec<(String, &str)> = vec![("lex.toml".to_string(), toml.as_str())];
    for (path, contents) in src_files {
        files.push((format!("src/{path}"), contents));
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    {
        let mut ar = tar::Builder::new(&mut enc);
        for (path, contents) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append_data(&mut header, path, contents.as_bytes()).unwrap();
        }
        ar.finish().unwrap();
    }
    enc.finish().unwrap()
}

#[test]
fn multi_file_publish_sees_earlier_files_own_update_in_same_request() {
    let (srv, _tmp) = start_server();

    let src_v1 = concat!(
        "fn counter() -> Int\n",
        "  examples {\n",
        "    counter() => 1,\n",
        "  }\n",
        "{ 1 }\n",
    );
    let archive_v1 = pkg_archive("multi", "0.1.0", &[("lib.lex", src_v1)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v1);
    assert_eq!(status, 200, "v1 publish must succeed, got: {body}");

    // v2: two files in ONE request, both redefining `counter` to the
    // SAME new body. File "a" is processed first — its diff against
    // the pre-request snapshot (body 1) sees a real change and emits
    // one modify_body op. File "b" is processed next — if the
    // snapshot correctly reflects file "a"'s just-published update
    // (body 2), file "b"'s diff sees no change (2 == 2) and emits
    // nothing further for `counter`.
    let src_v2 = concat!(
        "fn counter() -> Int\n",
        "  examples {\n",
        "    counter() => 2,\n",
        "  }\n",
        "{ 2 }\n",
    );
    let archive_v2 = pkg_archive("multi", "0.2.0", &[("a.lex", src_v2), ("b.lex", src_v2)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v2);
    assert_eq!(status, 200, "v2 publish must succeed, got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    let modify_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "modify_body")
        .collect();
    assert_eq!(
        modify_ops.len(), 1,
        "expected exactly one modify_body op (file a's diff sees the real \
         1->2 change; file b's diff should see its own already-published \
         2 and emit nothing) -- got {} modify_body ops: {:#?}",
        modify_ops.len(), ops,
    );
}

/// Each file's diff used to be computed as "this file's functions" vs.
/// the ENTIRE branch function set — so for a multi-file package, every
/// file's diff spuriously reported every function defined in every
/// OTHER file of the same package as "removed" (it's not defined in
/// *this* file). `store.publish_program` would dutifully emit a
/// `remove_function` op for it. This is what actually hit production:
/// lex-schema's 21-file publish removed a function that a later file in
/// the same request still legitimately modified, and the store's own
/// consistency check rejected the resulting diff outright (500:
/// "old_name_to_sig has no entry").
///
/// Reproduces the minimal shape: `helper` already exists (published in
/// v1). v2 is a single request with two files — `a.lex` (processed
/// first, alphabetically) doesn't mention `helper` at all; `b.lex`
/// legitimately redefines it. `helper` must survive: no
/// `remove_function` op anywhere in the response, and `b.lex`'s change
/// must land as a `modify_body`, not get miscounted as a fresh `add`.
#[test]
fn multi_file_publish_does_not_spuriously_remove_a_name_owned_by_another_file() {
    let (srv, _tmp) = start_server();

    let src_v1 = concat!(
        "fn helper() -> Int\n",
        "  examples {\n",
        "    helper() => 1,\n",
        "  }\n",
        "{ 1 }\n",
    );
    let archive_v1 = pkg_archive("multi2", "0.1.0", &[("lib.lex", src_v1)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v1);
    assert_eq!(status, 200, "v1 publish must succeed, got: {body}");

    let src_other = concat!(
        "fn other() -> Int\n",
        "  examples {\n",
        "    other() => 2,\n",
        "  }\n",
        "{ 2 }\n",
    );
    let src_helper_v2 = concat!(
        "fn helper() -> Int\n",
        "  examples {\n",
        "    helper() => 3,\n",
        "  }\n",
        "{ 3 }\n",
    );
    let archive_v2 = pkg_archive("multi2", "0.2.0", &[("a.lex", src_other), ("b.lex", src_helper_v2)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v2);
    assert_eq!(status, 200, "v2 publish must succeed (helper must not be spuriously removed), got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");

    let remove_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "remove_function")
        .collect();
    assert!(
        remove_ops.is_empty(),
        "expected no remove_function ops -- `helper` is untouched by file a.lex \
         and legitimately modified by b.lex, not removed. Got: {:#?}",
        remove_ops,
    );

    let modify_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "modify_body")
        .collect();
    assert_eq!(
        modify_ops.len(), 1,
        "expected exactly one modify_body op for helper's 1->3 change, got: {:#?}",
        ops,
    );

    let add_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "add_function")
        .collect();
    assert_eq!(
        add_ops.len(), 1,
        "expected exactly one add_function op for `other`, got: {:#?}",
        ops,
    );
}

/// The bug that actually hit production (#818): `pkg_publish_handler`
/// collapsed the branch's function set to one `FnDecl` per bare NAME
/// before diffing, even though `SigId` (which drives real identity)
/// already correctly disambiguates functions by their full signature,
/// not just their name. A real package (`lex-schema`) legitimately
/// declares three unrelated `validate` functions with different
/// signatures across `field.lex`, `schema.lex`, and `validator.lex` --
/// each file's own local helper, a completely normal pattern with no
/// language-level uniqueness requirement on top-level names across
/// files. Republishing all three (only one of them actually changed)
/// used to corrupt the diff: whichever two lost the name collision
/// were misdiagnosed, at best mis-tracked, at worst rejected outright
/// by the store's own consistency check.
///
/// This reproduces the minimal shape with three files, three `validate`
/// functions with three different signatures, republished with only
/// one body actually changed: the publish must succeed, with exactly
/// one `modify_body` op (the one that changed) and zero
/// `remove_function`/`add_function` ops (the other two are unchanged
/// and must be recognized as such, not as removed-and-re-added).
#[test]
fn multi_file_publish_disambiguates_same_name_different_signature_functions() {
    let (srv, _tmp) = start_server();

    // No `examples {}` blocks here on purpose: examples are part of
    // SigId's own hash (alongside effects/params/return type), so
    // field_v2 changing its example value to match a changed body would
    // ALSO change its structural key -- exercising a different, already
    //-understood trade-off (see this test module's doc comment) rather
    // than what this test means to demonstrate. A pure body change with
    // an unchanged signature must never affect structural matching.
    let field_v1 = "fn validate(x :: Int) -> Int { x }\n";
    let schema_v1 = "fn validate(x :: Str) -> Str { x }\n";
    let validator_v1 = "fn validate(x :: Bool) -> Bool { x }\n";
    let archive_v1 = pkg_archive("threevalidate", "0.1.0", &[
        ("field.lex", field_v1),
        ("schema.lex", schema_v1),
        ("validator.lex", validator_v1),
    ]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v1);
    assert_eq!(status, 200, "v1 publish (three same-name, different-signature functions) must succeed, got: {body}");

    // v2: republish all three files, only field.lex's body actually
    // changes (Int -> Int, x -> x + 1). schema.lex and validator.lex are
    // byte-for-byte the same as v1.
    let field_v2 = "fn validate(x :: Int) -> Int { x + 1 }\n";
    let archive_v2 = pkg_archive("threevalidate", "0.2.0", &[
        ("field.lex", field_v2),
        ("schema.lex", schema_v1),
        ("validator.lex", validator_v1),
    ]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v2);
    assert_eq!(status, 200, "v2 publish must succeed -- the three `validate`s must be correctly disambiguated by signature, got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");

    let modify_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "modify_body")
        .collect();
    assert_eq!(
        modify_ops.len(), 1,
        "expected exactly one modify_body op (field.lex's validate changed; \
         schema.lex's and validator.lex's did not), got: {:#?}",
        ops,
    );

    let remove_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "remove_function")
        .collect();
    assert!(
        remove_ops.is_empty(),
        "expected no remove_function ops -- all three `validate`s are still \
         declared in v2, just correctly disambiguated by signature. Got: {:#?}",
        remove_ops,
    );

    let add_ops: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "add_function")
        .collect();
    assert!(
        add_ops.is_empty(),
        "expected no add_function ops -- schema.lex's and validator.lex's \
         `validate` already existed from v1 and are unchanged in v2, they \
         must not be mistaken for new declarations. Got: {:#?}",
        add_ops,
    );
}

/// `pkg_publish_handler`'s branch is scoped to the whole TENANT, not to
/// one package -- confirmed in production, where `lex-schema` and
/// `lex-ocpi` share a tenant. An earlier version of this handler's fix
/// for #818 added a "genuinely removed" cleanup pass that treated every
/// branch function no file in the CURRENT archive claimed as deleted --
/// which, on a tenant hosting more than one package, meant every other
/// package's functions (never claimed by this package's own files) got
/// spuriously targeted for removal on every single publish. Caught
/// before it shipped further (a stale SigId made the cleanup's own
/// `diff_to_ops` call fail atomically before applying anything, so nothing
/// was actually lost), but the design itself was wrong and the pass was
/// removed rather than patched -- there's no package-scoped ownership
/// tracked anywhere to safely resurrect it.
///
/// This proves the fixed behavior directly: publish package "alpha",
/// then a completely unrelated package "bravo" under the SAME tenant --
/// bravo's publish must produce only its own op, nothing naming alpha's
/// function. Then republish alpha itself with a changed body: it must
/// show up as `modify_body` (proving its original function is still
/// live and resolvable on the branch), not `add_function` (which would
/// mean bravo's publish had wiped it out first).
#[test]
fn publishing_one_package_never_touches_an_unrelated_package_in_the_same_tenant() {
    let (srv, _tmp) = start_server();

    let alpha_v1 = "fn only_in_alpha() -> Int { 1 }\n";
    let archive_alpha_v1 = pkg_archive("alpha", "0.1.0", &[("lib.lex", alpha_v1)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_alpha_v1);
    assert_eq!(status, 200, "alpha v1 publish must succeed, got: {body}");

    let bravo_src = "fn only_in_bravo() -> Int { 2 }\n";
    let archive_bravo = pkg_archive("bravo", "0.1.0", &[("lib.lex", bravo_src)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_bravo);
    assert_eq!(status, 200, "bravo publish must succeed, got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    assert_eq!(
        ops.len(), 1,
        "bravo's publish must produce exactly its own one add_function op \
         and touch nothing belonging to alpha, got: {:#?}",
        ops,
    );
    assert_eq!(
        ops[0]["kind"]["op"], "add_function",
        "expected bravo's own add_function, got: {:#?}", ops[0],
    );

    // Republish alpha with a changed body. If bravo's publish had
    // spuriously removed alpha's function (the bug this test guards
    // against), this would show up as a fresh `add_function` instead.
    let alpha_v2 = "fn only_in_alpha() -> Int { 1 + 1 }\n";
    let archive_alpha_v2 = pkg_archive("alpha", "0.2.0", &[("lib.lex", alpha_v2)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_alpha_v2);
    assert_eq!(status, 200, "alpha v2 publish must succeed, got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    assert_eq!(
        ops.len(), 1,
        "expected exactly one op for alpha's changed body, got: {:#?}",
        ops,
    );
    assert_eq!(
        ops[0]["kind"]["op"], "modify_body",
        "alpha's function must still be live and resolvable as a modification \
         (bravo's publish must not have removed it) -- got: {:#?}", ops[0],
    );
}
