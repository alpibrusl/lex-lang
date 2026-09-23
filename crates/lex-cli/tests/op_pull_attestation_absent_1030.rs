//! #1030: `lex op pull` must not discard a fully-transferred pull over one
//! bad attestation reference.
//!
//! Repro (prod, 136k ops / 792 MB): ops + stages transferred cleanly, then
//! attestation sync hit `GET /v1/stage/<id>/attestations` -> `404 unknown
//! stage_id` for one stage and the whole pull aborted — the branch head was
//! never written locally despite everything else already landing on disk, so
//! a retry re-transferred the entire history.
//!
//! `stage_attestations_handler` 404s in exactly one case: `Store::get_metadata`
//! can't find the stage under any sig — a genuine absence (GC'd, never
//! persisted, or a stranded pre-#992 reference). These tests reproduce that
//! precisely by deleting a stage's `.metadata.json` on the server after a
//! real push (the content — `.ast.json` — is untouched, matching how the
//! ops+stages transfer having already fully succeeded in the real report),
//! and separately reproduce a genuine (non-absence) failure by corrupting an
//! attestation's stored JSON so the endpoint 500s instead of 404s.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

struct Server {
    addr: SocketAddr,
    root: std::path::PathBuf,
    _tmp: TempDir,
}

fn start_server() -> Server {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(root.clone()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(50));
    Server { addr, root, _tmp: tmp }
}

fn run(args: &[&str]) -> Output {
    Command::new(lex_bin()).args(args).output().unwrap()
}

/// Parse the FIRST JSON value on stdout. On a failed `--output json`
/// command, `emit_or_text`'s structured envelope (our own, carrying the
/// partial-success data) and `main`'s generic error envelope both land on
/// stdout back to back — we only need the first.
fn json(out: &Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut de = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    de.next()
        .unwrap_or_else(|| panic!("no JSON on stdout: {text}"))
        .unwrap_or_else(|e| panic!("bad JSON on stdout ({e}): {text}"))
}

/// (sig, stage) for the declaration named `name`, in the store at `root`.
fn locate(root: &Path, name: &str) -> (String, String) {
    for sig in std::fs::read_dir(root.join("stages")).unwrap() {
        let sig = sig.unwrap().path();
        let imp = sig.join("implementations");
        let Ok(rd) = std::fs::read_dir(&imp) else { continue };
        for f in rd {
            let f = f.unwrap().path();
            let fname = f.file_name().unwrap().to_string_lossy().into_owned();
            if let Some(stage) = fname.strip_suffix(".metadata.json") {
                let m: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap();
                if m["name"] == name {
                    return (
                        sig.file_name().unwrap().to_string_lossy().into_owned(),
                        stage.to_string(),
                    );
                }
            }
        }
    }
    panic!("no stage named {name} under {}", root.display());
}

/// Delete `<stage>.metadata.json` for `name` server-side, so
/// `Store::get_metadata` — and therefore `GET /v1/stage/<id>/attestations`
/// — can no longer find it, while its AST content is untouched.
fn drop_metadata(root: &Path, name: &str) -> String {
    let (sig, stage) = locate(root, name);
    let p = root
        .join("stages")
        .join(&sig)
        .join("implementations")
        .join(format!("{stage}.metadata.json"));
    std::fs::remove_file(&p).unwrap_or_else(|e| panic!("removing {}: {e}", p.display()));
    stage
}

/// Corrupt every attestation JSON blob for `stage_id`, so the server's
/// `list_for_stage` — and therefore the attestations endpoint — errors
/// instead of returning 404.
fn corrupt_attestations_for(root: &Path, stage_id: &str) -> usize {
    let stage_dir = root.join("attestations").join("by-stage").join(stage_id);
    let mut n = 0;
    for entry in std::fs::read_dir(&stage_dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", stage_dir.display()))
    {
        let att_id = entry.unwrap().file_name().to_string_lossy().into_owned();
        let primary = root.join("attestations").join(format!("{att_id}.json"));
        std::fs::write(&primary, b"{ not valid json").unwrap();
        n += 1;
    }
    n
}

fn publish_and_push(url: &str, store: &Path, src: &Path, src_text: &str) {
    std::fs::write(src, src_text).unwrap();
    let out = run(&[
        "publish", "--store", store.to_str().unwrap(), "--branch", "main",
        "--activate", src.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "publish failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = run(&["op", "push", url, "--store", store.to_str().unwrap()]);
    assert!(out.status.success(), "push failed: {}", String::from_utf8_lossy(&out.stderr));
}

const SRC: &str = "fn helper(x :: Int) -> Int { x + 1 }\nfn keep(x :: Int) -> Int { x * 3 }\n";

#[test]
fn pull_skips_a_genuinely_absent_stage_attestation_and_still_advances_head() {
    let srv = start_server();
    let url = format!("http://{}", srv.addr);
    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");

    publish_and_push(&url, &author, &src, SRC);

    // Simulate the #1030 condition: `helper`'s stage is genuinely absent
    // from the remote's store for attestation purposes (GC'd / never
    // persisted / stranded pre-#992 reference), while `keep`'s is intact.
    let dropped_stage = drop_metadata(&srv.root, "helper");

    let out = run(&["--output", "json", "op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "pull must succeed despite one absent-stage attestation: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let v = json(&out);
    let data = &v["data"];

    // Not aborted: the pull completed and reported the skip rather than
    // erroring out.
    let skipped = data["attestations_skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1, "expected exactly one skipped stage: {data}");
    assert_eq!(skipped[0]["stage_id"], dropped_stage);

    // The still-present stage's attestation (the hosted CI's TypeCheck
    // verdict) DID sync — one bad reference doesn't take down the rest.
    assert!(
        data["attestations_added"].as_u64().unwrap() >= 1,
        "keep's attestation should still have synced: {data}"
    );

    // Requirement: the branch head reflects what was committed, not unset.
    let tip = data["fast_forwarded_to"].as_str().expect("fast_forwarded_to must be set");
    assert!(!tip.is_empty());
    let head_path = consumer.join("branches").join("main.json");
    assert!(head_path.exists(), "local branch head file must be written");
    let head_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&head_path).unwrap()).unwrap();
    assert_eq!(head_json["head_op"], tip, "head file must match the reported tip");

    // A subsequent pull is a no-op fast-forward, not a full re-transfer.
    let out2 = run(&["--output", "json", "op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(out2.status.success());
    let v2 = json(&out2);
    assert_eq!(
        v2["data"]["received"].as_u64(),
        Some(0),
        "retry must not re-fetch ops that were already received"
    );
}

#[test]
fn pull_fails_loud_on_a_non_absence_attestation_error_but_still_commits() {
    let srv = start_server();
    let url = format!("http://{}", srv.addr);
    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");

    publish_and_push(&url, &author, &src, SRC);
    let (_sig, keep_stage) = locate(&srv.root, "keep");
    let n = corrupt_attestations_for(&srv.root, &keep_stage);
    assert!(n >= 1, "expected at least one hosted-CI attestation on `keep` to corrupt");

    let out = run(&["--output", "json", "op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "pull must fail loud on a non-404 attestation error: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v = json(&out);
    let data = &v["data"];
    assert_eq!(data["error"], "AttestationSyncFailed", "full response: {data}");
    let failed = data["attestations_failed"].as_array().expect("attestations_failed array");
    assert!(
        failed.iter().any(|f| f["stage_id"] == keep_stage),
        "the corrupted stage must be named in attestations_failed: {failed:?}"
    );
    // A genuine error must never be phrased as a skip (that's reserved for
    // confirmed absence).
    let skipped = data["attestations_skipped"].as_array().expect("attestations_skipped array");
    assert!(skipped.is_empty(), "a corrupt-data failure must not be reported as a skip: {skipped:?}");

    // Even though the command failed, ops + stages + the branch head must
    // still be committed (the "at minimum, non-fatal to committing what WAS
    // transferred" half of the fix) — a retry must not need to redo the
    // object transfer.
    let tip = data["fast_forwarded_to"].as_str().expect("fast_forwarded_to must still be set");
    let head_path = consumer.join("branches").join("main.json");
    assert!(
        head_path.exists(),
        "branch head must still have been written despite the attestation failure"
    );
    let head_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&head_path).unwrap()).unwrap();
    assert_eq!(head_json["head_op"], tip);

    // Because the branch head already advanced, a retry sees nothing new to
    // pull and succeeds as a no-op — it does NOT re-transfer the ops/stages
    // that already landed. (It also does not retry the corrupted
    // attestation, since attestation sync only runs for stages produced by
    // *newly received* ops; there is no dedicated attestation-only resync
    // path yet — see the PR description's follow-up.)
    let out2 = run(&["--output", "json", "op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(
        out2.status.success(),
        "retry must succeed as a no-op once the head is at tip: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out2.stdout),
        String::from_utf8_lossy(&out2.stderr)
    );
    let v2 = json(&out2);
    assert_eq!(
        v2["data"]["received"].as_u64(),
        Some(0),
        "retry must not re-fetch ops/stages that were already committed"
    );
}
