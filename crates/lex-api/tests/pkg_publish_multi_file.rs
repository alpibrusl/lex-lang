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

/// GET a path and return (status, body). Used to read the branch head
/// so a test can assert a request did (or did not) append to the op log.
fn get(addr: &SocketAddr, path: &str) -> (u16, String) {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    ).into_bytes();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match try_post(addr, &req) {
            Ok(result) => return result,
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    panic!("GET {path} failed after retries: {e}");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn branch_head(addr: &SocketAddr) -> Option<String> {
    let (status, body) = get(addr, "/v1/branches/main/head");
    assert_eq!(status, 200, "branch head probe must succeed, got: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    parsed["head_op"].as_str().map(|s| s.to_string())
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

/// A package is now loaded as ONE unit and every declaration carries its
/// file's mangling prefix (#828), so the scenario this test was written
/// for — two files in one request publishing under the SAME bare name,
/// where the second file's diff had to see the first's just-published
/// update rather than stale branch state — cannot arise: two files
/// declaring `counter` declare two differently-named functions.
///
/// What still has to hold, and is what this now pins: a function
/// modified in place across publishes reads as a modification of the
/// same function, not as a fresh add, and declaring the same bare name
/// in a second file does not disturb it.
#[test]
fn modifying_a_function_in_place_is_one_modify_not_an_add() {
    let (srv, _tmp) = start_server();

    let counter = |n: u8| -> String {
        format!(
            "fn counter() -> Int\n  examples {{\n    counter() => {n},\n  }}\n{{ {n} }}\n"
        )
    };

    let v1 = counter(1);
    let archive_v1 = pkg_archive("multi", "0.1.0", &[("a.lex", v1.as_str())]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v1);
    assert_eq!(status, 200, "v1 publish must succeed, got: {body}");

    // Same file, same name, new body — plus a SECOND file that declares
    // its own `counter`. The edit to `a.lex`'s function is one
    // modify_body; `b.lex`'s is a different function entirely, so it is
    // an add, and neither is mistaken for the other.
    let v2 = counter(2);
    let archive_v2 = pkg_archive(
        "multi",
        "0.2.0",
        &[("a.lex", v2.as_str()), ("b.lex", v2.as_str())],
    );
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v2);
    assert_eq!(status, 200, "v2 publish must succeed, got: {body}");

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    let of_kind = |k: &str| -> Vec<&serde_json::Value> {
        ops.iter().filter(|op| op["kind"]["op"] == k).collect()
    };
    assert_eq!(
        of_kind("modify_body").len(), 1,
        "a.lex's `counter` changed 1 -> 2 in place: exactly one modify_body, got: {ops:#?}",
    );
    assert_eq!(
        of_kind("add_function").len(), 1,
        "b.lex's `counter` is its own function under its own prefix: one add_function, \
         got: {ops:#?}",
    );
    assert!(
        of_kind("remove_function").is_empty(),
        "nothing was removed, got: {ops:#?}",
    );

    // The two `counter`s really are distinct published names.
    let (status, body) = get(&srv.addr, "/v1/pkg/multi/0.2.0");
    assert_eq!(status, 200, "record must be readable, got: {body}");
    let rec: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let names: Vec<&str> = rec["function_names"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(names.len(), 2, "two distinct functions, got: {names:?}");
    assert!(
        names.iter().all(|n| n.ends_with(".counter")) && names[0] != names[1],
        "both are prefix-mangled `counter`s under different prefixes, got: {names:?}",
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
/// Reproduces the minimal shape: `b.lex` owns `helper` and keeps owning
/// it across both versions, while `a.lex` never mentions it. `helper`
/// must survive: no `remove_function` anywhere, and `b.lex`'s edit lands
/// as a `modify_body`, not as a fresh add.
///
/// (Before #828 the two versions could put `helper` in *different* files
/// and still be one function, because a file's own declarations were
/// published under their bare names. Now a declaration belongs to its
/// file, so "the same function" means the same file — moving a function
/// between files is a new function plus an orphan, which is the
/// documented cost of the single-pass load.)
#[test]
fn multi_file_publish_does_not_spuriously_remove_a_name_owned_by_another_file() {
    let (srv, _tmp) = start_server();

    let src_other = concat!(
        "fn other() -> Int\n",
        "  examples {\n",
        "    other() => 2,\n",
        "  }\n",
        "{ 2 }\n",
    );
    let src_helper_v1 = concat!(
        "fn helper() -> Int\n",
        "  examples {\n",
        "    helper() => 1,\n",
        "  }\n",
        "{ 1 }\n",
    );
    let archive_v1 = pkg_archive("multi2", "0.1.0", &[("b.lex", src_helper_v1)]);
    let (status, body) = post_bytes(&srv.addr, "/v1/pkg/publish", &archive_v1);
    assert_eq!(status, 200, "v1 publish must succeed, got: {body}");

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

// ── #826: republishing unchanged source must be a no-op ──────────────────────

/// Two files, one importing the other — the shape every real multi-file
/// package has, and the shape #826 broke.
const IDEMPOTENT_SRC: &[(&str, &str)] = &[
    (
        "error.lex",
        concat!(
            "fn code_missing() -> Str\n",
            "  examples {\n",
            "    code_missing() => \"missing\",\n",
            "  }\n",
            "{ \"missing\" }\n",
        ),
    ),
    (
        "schema.lex",
        concat!(
            "import \"./error\" as e\n",
            "fn describe() -> Str\n",
            "  examples {\n",
            "    describe() => \"missing\",\n",
            "  }\n",
            "{ e.code_missing() }\n",
        ),
    ),
];

/// #826: `pkg_publish_handler` unpacks each archive into a fresh temp
/// dir, and the loader's mangling prefix hashed the file's *absolute*
/// path — so `error.lex`'s functions came back named
/// `error_2ee49b9f.code_missing` on one request and
/// `error_665fe47f.code_missing` on the next, from byte-identical
/// source. Every name reached through a local import therefore diffed as
/// brand new on every publish: real `add_function` ops, the previous
/// request's versions orphaned, and the branch's live function map
/// growing without bound forever (observed on the real tenant: 3,664 →
/// 4,477 entries across one supposedly-no-op publish).
///
/// Republishing identical source must produce no ops at all and leave
/// the branch head exactly where it was.
#[test]
fn republishing_identical_source_emits_no_ops() {
    let (srv, _tmp) = start_server();

    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("idem", "0.1.0", IDEMPOTENT_SRC),
    );
    assert_eq!(status, 200, "first publish must succeed, got: {body}");
    let head_after_first = branch_head(&srv.addr);
    assert!(head_after_first.is_some(), "first publish must move the branch head");

    // Same source, new version number (the same version is rejected
    // outright — see the 409 test below).
    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("idem", "0.2.0", IDEMPOTENT_SRC),
    );
    assert_eq!(status, 200, "republish must succeed, got: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    assert!(
        ops.is_empty(),
        "republishing byte-identical source must emit zero ops, got {}: {:#?}",
        ops.len(), ops,
    );
    assert_eq!(
        branch_head(&srv.addr), head_after_first,
        "a no-op republish must not move the branch head",
    );
}

/// The names a publish records must also be stable across requests —
/// not merely self-consistent within one. If the second publish emitted
/// no ops but under a *different* set of names, the first publish's
/// functions would be the orphans #826 describes.
#[test]
fn republished_function_names_are_identical_across_requests() {
    let (srv, _tmp) = start_server();

    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("names", "0.1.0", IDEMPOTENT_SRC),
    );
    assert_eq!(status, 200, "first publish must succeed, got: {body}");
    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("names", "0.2.0", IDEMPOTENT_SRC),
    );
    assert_eq!(status, 200, "republish must succeed, got: {body}");

    let names_of = |version: &str| -> Vec<String> {
        let (status, body) = get(&srv.addr, &format!("/v1/pkg/names/{version}"));
        assert_eq!(status, 200, "record for {version} must be readable, got: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        let mut names: Vec<String> = parsed["function_names"].as_array()
            .expect("function_names array")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        names.sort();
        names
    };
    let v1 = names_of("0.1.0");
    let v2 = names_of("0.2.0");
    assert!(
        v1.iter().any(|n| n.starts_with("error_") && n.ends_with(".code_missing")),
        "expected a mangled name for the locally-imported file, got: {v1:?}",
    );
    assert_eq!(
        v1, v2,
        "the same source must publish under the same names on every request (#826)",
    );
}

/// A duplicate `(name, version)` publish is rejected with 409 — but the
/// check used to run *after* the publish loop had already written every
/// file's ops to the store. The client saw a clean 409 while the
/// tenant's op log and function set had already grown, which is how
/// #826's repeated "duplicate version" verification publishes kept
/// inflating the tenant. A rejected publish must leave the store
/// untouched.
#[test]
fn duplicate_version_publish_leaves_the_op_log_untouched() {
    let (srv, _tmp) = start_server();

    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("dup", "0.1.0", IDEMPOTENT_SRC),
    );
    assert_eq!(status, 200, "first publish must succeed, got: {body}");
    let head_after_first = branch_head(&srv.addr);

    // Same version, genuinely different content: without the re-ordered
    // check this would publish the change and *then* 409.
    let changed: &[(&str, &str)] = &[(
        "error.lex",
        concat!(
            "fn code_missing() -> Str\n",
            "  examples {\n",
            "    code_missing() => \"gone\",\n",
            "  }\n",
            "{ \"gone\" }\n",
        ),
    )];
    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("dup", "0.1.0", changed),
    );
    assert_eq!(status, 409, "duplicate version must be rejected, got: {body}");
    assert_eq!(
        branch_head(&srv.addr), head_after_first,
        "a 409'd publish must not have appended any ops",
    );
}



// ── #828: one pass over the package, not one per importing file ──────────────

/// The shape that made this expensive, in miniature: one shared file
/// (`error.lex`) imported by every other file in the package.
///
/// `pkg_publish_handler` used to load each top-level file independently,
/// and each of those loads flattened in the whole local-import closure —
/// so `error.lex`'s declarations were canonicalized, type-checked,
/// diffed, and run through `publish_program` once per importing file. On
/// the real 21-file `lex-schema` package, whose `error.lex` is imported
/// by 17 of its files, that was 2,239 `FnDecl`s processed for 693
/// distinct names, and 21 separate `publish_program` calls each reading
/// every live function on the branch (#828).
///
/// Pinned here by counting what reaches the store: one `add_function` per
/// declaration in the package, no more.
#[test]
fn each_declaration_is_published_exactly_once_however_many_files_import_it() {
    let (srv, _tmp) = start_server();

    let shared = concat!(
        "fn code_one() -> Str\n  examples {\n    code_one() => \"one\",\n  }\n{ \"one\" }\n",
        "fn code_two() -> Str\n  examples {\n    code_two() => \"two\",\n  }\n{ \"two\" }\n",
        "fn code_three() -> Str\n  examples {\n    code_three() => \"three\",\n  }\n{ \"three\" }\n",
    );
    // Five importers, each with one declaration of its own: 3 + 5 = 8
    // declarations in the package, and `error.lex` reached five times.
    let importer = |n: &str| -> String {
        format!(
            "import \"./error\" as e\nfn use_{n}() -> Str\n  examples {{\n    use_{n}() => \"one\",\n  }}\n{{ e.code_one() }}\n"
        )
    };
    let bodies: Vec<String> = ["a", "b", "c", "d", "f"].iter().map(|n| importer(n)).collect();
    let mut files: Vec<(&str, &str)> = vec![("error.lex", shared)];
    for (name, body) in [("a.lex", 0), ("b.lex", 1), ("c.lex", 2), ("d.lex", 3), ("f.lex", 4)] {
        files.push((name, bodies[body].as_str()));
    }

    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("dense", "0.1.0", &files),
    );
    assert_eq!(status, 200, "publish must succeed, got: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array in publish response");
    let adds: Vec<&serde_json::Value> = ops.iter()
        .filter(|op| op["kind"]["op"] == "add_function")
        .collect();
    assert_eq!(
        adds.len(), 8,
        "8 declarations in the package means 8 add_function ops; a per-importing-file \
         load re-published `error.lex`'s three functions once per importer. Got {}: {:#?}",
        adds.len(), ops,
    );

    // And the published name set holds each declaration once.
    let (status, body) = get(&srv.addr, "/v1/pkg/dense/0.1.0");
    assert_eq!(status, 200, "record must be readable, got: {body}");
    let rec: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    let names: Vec<String> = rec["function_names"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap().to_string()).collect();
    let unique: std::collections::BTreeSet<&String> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "no name is published twice, got: {names:?}");
    assert_eq!(names.len(), 8, "one name per declaration, got: {names:?}");
    let from_shared: Vec<&String> = names.iter().filter(|n| n.starts_with("error_")).collect();
    assert_eq!(
        from_shared.len(), 3,
        "`error.lex`'s three functions appear once each, not once per importer, got: {names:?}",
    );
    // Nothing is published under a bare source-level name any more: every
    // declaration belongs to the file it was declared in.
    assert!(
        names.iter().all(|n| n.contains('.')),
        "every published name carries its file's prefix, got: {names:?}",
    );
}

/// Imports stay attributed to the file that declares them. A flattened
/// per-file load could not tell a file's own imports from those of
/// everything it imported, so every importer of a file that used
/// `std.str` looked like it imported `std.str` itself.
#[test]
fn imports_are_attributed_to_the_file_that_declares_them() {
    let (srv, _tmp) = start_server();

    let files: Vec<(&str, &str)> = vec![
        (
            "helper.lex",
            "import \"std.str\" as str\nfn shout(s :: Str) -> Str\n  examples {\n    shout(\"a\") => \"A\",\n  }\n{ str.to_upper(s) }\n",
        ),
        (
            "main.lex",
            "import \"./helper\" as h\nfn go() -> Str\n  examples {\n    go() => \"A\",\n  }\n{ h.shout(\"a\") }\n",
        ),
    ];
    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("attrib", "0.1.0", &files),
    );
    assert_eq!(status, 200, "publish must succeed, got: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON response");
    let ops = parsed["ops"].as_array().expect("ops array");
    let import_files: Vec<&str> = ops.iter()
        .filter(|op| op["kind"]["op"] == "add_import")
        .map(|op| op["kind"]["in_file"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        import_files, vec!["src/helper.lex"],
        "`std.str` is imported by helper.lex alone; main.lex merely imports helper. \
         Got: {ops:#?}",
    );
}

/// A type error anywhere in the package publishes nothing at all. Before
/// #828 each file was published as it was processed, so a failure in a
/// later file left earlier files' ops already applied.
#[test]
fn a_type_error_anywhere_publishes_nothing() {
    let (srv, _tmp) = start_server();
    let head_before = branch_head(&srv.addr);

    let files: Vec<(&str, &str)> = vec![
        ("a_ok.lex", "fn fine() -> Int\n  examples {\n    fine() => 1,\n  }\n{ 1 }\n"),
        // `z_bad.lex` sorts last, so it is reached only after `a_ok.lex`
        // would have been published under the old per-file loop.
        ("z_bad.lex", "fn broken() -> Int { \"not an int\" }\n"),
    ];
    let (status, body) = post_bytes(
        &srv.addr,
        "/v1/pkg/publish",
        &pkg_archive("atomic", "0.1.0", &files),
    );
    assert_eq!(status, 422, "a type error must be rejected, got: {status} {body}");
    assert_eq!(
        branch_head(&srv.addr), head_before,
        "a rejected publish must leave the branch head where it was",
    );
}
