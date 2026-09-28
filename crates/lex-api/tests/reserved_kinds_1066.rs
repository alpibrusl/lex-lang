//! #1066: `State::reserved_kinds` keeps clients from filing attestations of
//! a reserved KIND through `POST /v1/attestations/batch`, whatever
//! `produced_by.tool` they claim. Readers such as
//! `Store::latest_review_verdict` key on the kind, so the producer
//! reservation alone (#1053) does not protect review verdicts.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::{State, REVIEW_KIND, TYPE_CHECK_KIND};
use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor,
    ReviewVerdict,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn start_with(tmp: &TempDir, kinds: Vec<String>, producers: Vec<String>) -> SocketAddr {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(
        State::open(tmp.path().to_path_buf())
            .unwrap()
            .with_reserved_kinds(kinds)
            .with_reserved_producers(producers),
    );
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    addr
}

fn start(tmp: &TempDir, kinds: Vec<String>) -> SocketAddr {
    start_with(tmp, kinds, Vec::new())
}

fn review_only() -> Vec<String> {
    vec![REVIEW_KIND.to_string()]
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

fn producer(tool: &str) -> ProducerDescriptor {
    ProducerDescriptor { tool: tool.into(), version: "0".into(), model: None }
}

fn type_check(tool: &str, stage: &str) -> Attestation {
    Attestation::with_timestamp(
        stage.to_string(), None, None,
        AttestationKind::TypeCheck, AttestationResult::Passed,
        producer(tool), None, 1,
    )
}

fn review(tool: &str, result: AttestationResult, stage: &str) -> Attestation {
    Attestation::with_timestamp(
        stage.to_string(), None, None,
        AttestationKind::Review {
            reviewer: "gh:alice".into(),
            verdict: ReviewVerdict::Approve,
            notes: None,
        },
        result, producer(tool), None, 1,
    )
}

fn batch(addr: &SocketAddr, atts: &[Attestation]) -> (u16, String) {
    http(addr, "POST", "/v1/attestations/batch", &serde_json::to_string(atts).unwrap())
}

fn all(tmp: &TempDir) -> Vec<Attestation> {
    AttestationLog::open(tmp.path()).unwrap().list_all().unwrap()
}

fn body_json(b: &str) -> Value {
    serde_json::from_str(b).unwrap_or_else(|e| panic!("not json ({e}): {b}"))
}

const HUB: &str = lex_store::HUB_CI_PRODUCER_TOOL;

fn results() -> [AttestationResult; 3] {
    [
        AttestationResult::Passed,
        AttestationResult::Failed { detail: "no".into() },
        AttestationResult::Inconclusive { detail: "maybe".into() },
    ]
}

#[test]
fn a_review_kind_is_refused_whole_for_every_result_and_any_producer_name() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, review_only());
    // One legitimate attestation already in the log: "unchanged" is exact.
    let (s, b) = batch(&addr, &[type_check("some-ci", "s0")]);
    assert_eq!(s, 200, "{b}");
    let before = all(&tmp);
    assert_eq!(before.len(), 1);

    for result in results() {
        // The very probe from the issue: an unreserved tool name.
        for tool in ["evil-tool", "", "lex-store::review:alice", HUB] {
            let forged = review(tool, result.clone(), "s1");
            let (s, b) = batch(&addr, std::slice::from_ref(&forged));
            assert_eq!(s, 403, "{tool:?} {result:?}: {b}");
            let v = body_json(&b);
            assert_eq!(v["error"], "ReservedKind", "{b}");
            assert_eq!(v["detail"]["attestation_id"], json!(forged.attestation_id), "{b}");
            assert_eq!(v["detail"]["kind"], "review", "{b}");
            assert_eq!(v["detail"]["reserved_kind"], "review", "{b}");
            assert!(v["detail"]["message"].as_str().unwrap().contains("reserved"), "{b}");
        }
    }
    assert_eq!(all(&tmp), before, "a refused batch writes nothing");
}

#[test]
fn a_mixed_batch_is_refused_whole_in_either_order() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, review_only());
    let legit = type_check("some-ci", "s1");
    let forged = review("evil-tool", AttestationResult::Passed, "s2");
    let (s, b) = batch(&addr, &[legit.clone(), forged.clone()]);
    assert_eq!(s, 403, "{b}");
    assert!(b.contains("ReservedKind"), "{b}");
    let (s, b) = batch(&addr, &[forged, legit]);
    assert_eq!(s, 403, "{b}");
    assert!(b.contains("ReservedKind"), "{b}");
    assert!(all(&tmp).is_empty(), "the legitimate half must not be written either");
}

#[test]
fn an_ordinary_batch_is_accepted_when_review_is_reserved() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, review_only());
    let (s, b) = batch(&addr, &[type_check("some-ci", "s1"), type_check("another", "s2")]);
    assert_eq!(s, 200, "{b}");
    assert_eq!(body_json(&b)["added"], 2);
    assert_eq!(all(&tmp).len(), 2);
}

#[test]
fn default_empty_reserves_nothing_so_the_same_forged_batch_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, Vec::new());
    let forged = review("evil-tool", AttestationResult::Passed, "s1");
    let (s, b) = batch(&addr, &[forged.clone(), type_check("some-ci", "s1")]);
    assert_eq!(s, 200, "default must accept the batch the reservation refuses: {b}");
    assert!(all(&tmp).iter().any(|a| a.attestation_id == forged.attestation_id));
    // And through a State that never called the builder at all.
    assert!(State::open(TempDir::new().unwrap().path().to_path_buf()).unwrap().reserved_kinds.is_empty());
}

#[test]
fn kind_matching_is_trimmed_case_folded_and_prefixable_like_producers() {
    // (entries, does a Review get refused?)
    let cases: Vec<(Vec<&str>, bool)> = vec![
        (vec!["review"], true),
        (vec!["Review"], true),
        (vec!["  REVIEW\t"], true),
        (vec!["rev*"], true),
        (vec![" Rev* "], true),
        (vec!["", "  ", "review"], true),
        // Not matches:
        (vec!["rev"], false),           // an exact entry is not a prefix
        (vec!["reviews"], false),
        (vec!["review-extra*"], false),
        (vec!["type_check"], false),    // a different kind
        (vec!["*"], false),             // bare `*` is an empty prefix: reserves nothing (as for producers)
        (vec!["", "   "], false),       // blanks reserve nothing
    ];
    for (entries, refused) in cases {
        let tmp = TempDir::new().unwrap();
        let addr = start(&tmp, entries.iter().map(|s| s.to_string()).collect());
        let (s, b) = batch(&addr, &[review("evil-tool", AttestationResult::Passed, "s1")]);
        assert_eq!(s == 403, refused, "{entries:?}: {s} {b}");
        assert_eq!(all(&tmp).is_empty(), refused, "{entries:?}");
        if refused {
            // The entry is echoed exactly as configured.
            let v = body_json(&b);
            assert_eq!(v["detail"]["reserved_kind"], json!(entries.iter().find(|e| !e.trim().is_empty()).unwrap()));
        }
    }
    // `TypeCheck` and the serde tag `type_check` name the same kind.
    for entry in ["TypeCheck", "type_check", "TYPE_CHECK", "type_*", "typecheck"] {
        let tmp = TempDir::new().unwrap();
        let addr = start(&tmp, vec![entry.to_string()]);
        let (s, b) = batch(&addr, &[type_check("some-ci", "s1")]);
        assert_eq!(s, 403, "{entry}: {b}");
        assert_eq!(body_json(&b)["detail"]["kind"], TYPE_CHECK_KIND);
        // ...and leaves the other kinds alone.
        let (s, _) = batch(&addr, &[review("some-ci", AttestationResult::Passed, "s1")]);
        assert_eq!(s, 200, "{entry}");
    }
}

#[test]
fn reserved_kinds_and_reserved_producers_compose_and_the_kind_answers_first() {
    let tmp = TempDir::new().unwrap();
    let producers = vec![HUB.to_string(), lex_store::REVIEW_PRODUCER_RESERVATION.to_string()];
    let addr = start_with(&tmp, review_only(), producers);

    // Only the producer is reserved -> ReservedProducer.
    let (s, b) = batch(&addr, &[type_check(HUB, "s1")]);
    assert_eq!((s, body_json(&b)["error"].as_str().unwrap().to_string()), (403, "ReservedProducer".into()), "{b}");
    // Only the kind is reserved (unreserved producer) -> ReservedKind.
    let (s, b) = batch(&addr, &[review("evil-tool", AttestationResult::Passed, "s1")]);
    assert_eq!((s, body_json(&b)["error"].as_str().unwrap().to_string()), (403, "ReservedKind".into()), "{b}");
    // One attestation violating both -> ReservedKind wins.
    let (s, b) = batch(&addr, &[review("lex-store::review:alice", AttestationResult::Passed, "s1")]);
    assert_eq!((s, body_json(&b)["error"].as_str().unwrap().to_string()), (403, "ReservedKind".into()), "{b}");
    // A batch whose FIRST attestation violates the producer and a LATER one
    // the kind -> ReservedKind (the kind pass runs over the whole batch first).
    let (s, b) = batch(&addr, &[type_check(HUB, "s1"), review("evil-tool", AttestationResult::Passed, "s2")]);
    assert_eq!((s, body_json(&b)["error"].as_str().unwrap().to_string()), (403, "ReservedKind".into()), "{b}");
    assert!(all(&tmp).is_empty());
    // Neither violated -> accepted.
    let (s, b) = batch(&addr, &[type_check("some-ci", "s1")]);
    assert_eq!(s, 200, "{b}");
}

#[test]
fn reserved_kind_check_runs_after_parse_but_before_id_and_op_validation() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, review_only());
    // Malformed body: 400 (nothing to judge).
    let (s, _) = http(&addr, "POST", "/v1/attestations/batch", "not json");
    assert_eq!(s, 400);
    // A Review with a wrong id, and one that names an unknown op: the kind
    // refusal (403) answers before the 409 / 422 validations would.
    let mut bad_id = review("evil-tool", AttestationResult::Passed, "s1");
    bad_id.attestation_id = type_check("x", "y").attestation_id;
    let (s, b) = batch(&addr, &[bad_id]);
    assert_eq!(s, 403, "{b}");
    // The same wrong id on an unreserved kind is still the 409.
    let mut bad_id = type_check("some-ci", "s1");
    bad_id.attestation_id = type_check("x", "y").attestation_id;
    let (s, b) = batch(&addr, &[bad_id]);
    assert_eq!(s, 409, "{b}");
    assert!(all(&tmp).is_empty());
}

/// The kind is judged on the PARSED attestation, i.e. exactly what the store
/// persists (the record is re-serialised from the parsed value), so no body
/// shape can persist a reserved kind without being judged as it. With the
/// serde version we build against, the derived `Attestation` (internally
/// tagged `kind`) does not even parse a duplicate `"kind"` key — a hosted
/// embedder's raw-body guard inspects every duplicate because it cannot rely
/// on that; the native check does not need to. Pin both facts: the duplicate
/// bodies are refused `400` (never stored), and whatever would ever parse is
/// judged by its parsed kind (`a_review_kind_is_refused_...` above).
#[test]
fn duplicate_or_variant_kind_tags_can_never_store_a_reserved_kind() {
    let review_json = serde_json::to_string(&review("evil-tool", AttestationResult::Passed, "s1")).unwrap();
    let tc_json = serde_json::to_string(&type_check("some-ci", "s1")).unwrap();
    assert!(review_json.contains(r#""kind":{"kind":"review""#), "{review_json}");
    assert!(tc_json.contains(r#""kind":{"kind":"type_check"}"#), "{tc_json}");

    let mut bodies: Vec<(String, String)> = vec![
        // Review first, type_check second (and the reverse), inner tag.
        ("dup tag, review first".into(),
         review_json.replacen(r#""kind":"review""#, r#""kind":"review","kind":"type_check""#, 1)),
        ("dup tag, type_check first".into(),
         tc_json.replacen(r#""kind":"type_check""#, r#""kind":"type_check","kind":"review""#, 1)),
        // Duplicate outer `kind` field.
        ("dup outer field, review last".into(),
         tc_json.replacen(r#""kind":{"kind":"type_check"}"#,
             r#""kind":{"kind":"type_check"},"kind":{"kind":"review","reviewer":"a","verdict":"approve"}"#, 1)),
        ("dup outer field, review first".into(),
         tc_json.replacen(r#""kind":{"kind":"type_check"}"#,
             r#""kind":{"kind":"review","reviewer":"a","verdict":"approve"},"kind":{"kind":"type_check"}"#, 1)),
    ];
    // Case / whitespace variants of the tag are not `Review` to serde.
    for tag in ["Review", " review", "REVIEW", "review "] {
        bodies.push((format!("variant tag {tag:?}"),
            review_json.replacen(r#""kind":"review""#, &format!(r#""kind":"{tag}""#), 1)));
    }

    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, review_only());
    for (what, body) in &bodies {
        let (s, resp) = http(&addr, "POST", "/v1/attestations/batch", &format!("[{body}]"));
        assert_eq!(s, 400, "{what}: {resp}");
    }
    assert!(all(&tmp).is_empty(), "nothing smuggled behind a duplicate/variant tag may be stored");
    // Without the reservation the same bodies are refused identically: it is
    // serde, not the guard, that rejects them (so the guard is not what makes
    // this safe, and it cannot be bypassed through it).
    let tmp2 = TempDir::new().unwrap();
    let addr2 = start(&tmp2, Vec::new());
    for (what, body) in &bodies {
        let (s, resp) = http(&addr2, "POST", "/v1/attestations/batch", &format!("[{body}]"));
        assert_eq!(s, 400, "{what}: {resp}");
    }
    assert!(all(&tmp2).is_empty());
}

/// Publish a tiny program and return (stage_id, head_op).
fn publish(addr: &SocketAddr) -> (String, String) {
    let (s, b) = http(addr, "POST", "/v1/publish",
        &json!({"source": "fn foo(n :: Int) -> Int { n }\n", "activate": false}).to_string());
    assert_eq!(s, 200, "{b}");
    let v: Value = serde_json::from_str(&b).unwrap();
    let stage_id = v["ops"][0]["kind"]["stage_id"].as_str()
        .unwrap_or_else(|| panic!("no stage id in {b}")).to_string();
    let head = v["head_op"].as_str().unwrap_or_else(|| panic!("no head_op in {b}")).to_string();
    (stage_id, head)
}

#[test]
fn server_side_writers_still_write_when_review_and_type_check_are_reserved() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![REVIEW_KIND.to_string(), TYPE_CHECK_KIND.to_string()]);

    // verify_head_and_attest via a real head push.
    let (s, b) = http(&addr, "POST", "/v1/branches", &json!({"name": "target", "from": "main"}).to_string());
    assert!(s == 200 || s == 201, "{b}");
    let (stage_id, head) = publish(&addr);
    let (s, b) = http(&addr, "POST", "/v1/branches/target/head", &json!({"head_op": head}).to_string());
    assert_eq!(s, 200, "{b}");
    assert_eq!(body_json(&b)["ci"]["passed"], json!(true), "{b}");
    let (s, b) = http(&addr, "GET", &format!("/v1/stage/{stage_id}/attestations"), "");
    assert_eq!(s, 200, "{b}");
    let hub_type_check = body_json(&b)["attestations"].as_array().unwrap().iter().any(|a| {
        a["produced_by"]["tool"] == HUB && a["kind"]["kind"] == json!("type_check")
            && a["result"]["result"] == json!("passed")
    });
    assert!(hub_type_check, "the hub's own TypeCheck must be written despite the reservation: {b}");

    // POST /v1/review/verdict (record_review).
    let (s, b) = http(&addr, "POST", "/v1/review/verdict", &json!({
        "stage_id": stage_id, "verdict": "reject", "reviewer": "alice"}).to_string());
    assert_eq!(s, 201, "{b}");
    assert!(
        all(&tmp).iter().any(|a| a.stage_id == stage_id && matches!(a.kind, AttestationKind::Review { .. })),
        "record_review must bypass the kind reservation"
    );
    assert_eq!(
        lex_store::Store::open(tmp.path()).unwrap().latest_review_verdict(&stage_id).unwrap(),
        Some(ReviewVerdict::Reject)
    );

    // While the same kinds over HTTP are refused, whatever the producer.
    let (s, _) = batch(&addr, &[review("evil-tool", AttestationResult::Passed, &stage_id)]);
    assert_eq!(s, 403);
    let (s, _) = batch(&addr, &[type_check(HUB, &stage_id)]);
    assert_eq!(s, 403);
    let verdict = lex_store::Store::open(tmp.path()).unwrap().latest_review_verdict(&stage_id).unwrap();
    assert_eq!(verdict, Some(ReviewVerdict::Reject), "the forged Approve did not flip the verdict");
}
