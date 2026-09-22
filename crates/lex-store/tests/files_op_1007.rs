//! #1007 PR 2: `SetFiles { manifest }` — files ordered in the op DAG but
//! never part of the program.

use std::collections::BTreeSet;

use lex_store::files::{Entry, Manifest, ManifestAt, ManifestError, NotReplayable};
use lex_store::DEFAULT_BRANCH as MAIN;
use lex_store::{Operation, OperationKind, OperationRecord, StageTransition, Store, StoreError};
use lex_vcs::OpLog;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn head(s: &Store) -> Option<String> {
    s.get_branch(MAIN).unwrap().and_then(|b| b.head_op)
}

fn add(s: &Store, sig: &str, stg: &str) -> String {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stg.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        head(s),
    );
    let t = StageTransition::Create {
        sig_id: sig.into(),
        stage_id: stg.into(),
    };
    s.apply_operation(MAIN, op, t).unwrap()
}

fn modify(s: &Store, sig: &str, from: &str, to: &str) -> String {
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        head(s),
    );
    let t = StageTransition::Replace {
        sig_id: sig.into(),
        from: from.into(),
        to: to.into(),
    };
    s.apply_operation(MAIN, op, t).unwrap()
}

/// Store the given files as blobs plus their manifest; return the id.
fn files(s: &Store, fs: &[(&str, &[u8])]) -> String {
    let mut m = Manifest::new();
    for (path, bytes) in fs {
        let blob = s.put_blob_bytes(bytes).unwrap();
        m.entries.insert(
            path.to_string(),
            Entry {
                blob,
                mode: "100644".into(),
                size: bytes.len() as u64,
            },
        );
    }
    s.put_manifest(&m).unwrap()
}

fn set(id: &str) -> ManifestAt {
    ManifestAt::Set {
        manifest: id.to_string(),
    }
}

fn op_count(s: &Store) -> usize {
    OpLog::open(s.root()).unwrap().list_all().unwrap().len()
}

/// Forget the cached head view so the next read is a full walk.
fn drop_snapshot(s: &Store) {
    let p = s
        .root()
        .join("branches")
        .join(format!("{MAIN}.head_snapshot.json"));
    let _ = std::fs::remove_file(p);
}

#[test]
fn set_files_leaves_the_head_sig_stage_map_untouched() {
    let (s, _tmp) = fresh();
    add(&s, "f", "f1");
    let before_op = add(&s, "g", "g1");
    let map_before = s.branch_head(MAIN).unwrap();
    assert_eq!(map_before.len(), 2);

    let m = files(
        &s,
        &[
            ("README.md", b"# hi\n"),
            ("logo.png", b"\x89PNG\r\n\x1a\n\x00\xff"),
        ],
    );
    let op = s
        .apply_set_files(MAIN, &m, Some(&"intent-readme".to_string()))
        .unwrap();

    // The head moved to a SetFiles op recorded as FilesOnly, with the
    // intent carried like any other op...
    assert_eq!(head(&s).as_deref(), Some(op.as_str()));
    let rec = OpLog::open(s.root()).unwrap().get(&op).unwrap().unwrap();
    assert_eq!(
        rec.op.kind,
        OperationKind::SetFiles {
            manifest: m.clone()
        }
    );
    assert!(!rec.op.kind.is_semantic());
    assert_eq!(rec.op.parents, vec![before_op]);
    assert_eq!(rec.op.intent_id.as_deref(), Some("intent-readme"));
    assert_eq!(rec.produces, StageTransition::FilesOnly);

    // ...and the program is exactly what it was, via the cached and the
    // full-walk paths alike.
    assert_eq!(s.branch_head(MAIN).unwrap(), map_before);
    drop_snapshot(&s);
    assert_eq!(s.branch_head(MAIN).unwrap(), map_before);
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m));
    assert_eq!(s.manifest_at(&op).unwrap(), set(&m));

    // The record round-trips through its on-disk JSON shape.
    let json = serde_json::to_string(&rec).unwrap();
    assert!(json.contains(r#""op":"set_files""#), "{json}");
    assert!(
        json.contains(r#""produces":{"kind":"files_only"}"#),
        "{json}"
    );
    let back: OperationRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rec);
}

#[test]
fn manifest_is_inherited_across_semantic_ops() {
    let (s, _tmp) = fresh();
    let genesis = add(&s, "f", "f1");
    assert_eq!(s.manifest_at(&genesis).unwrap(), ManifestAt::Absent);
    assert_eq!(s.branch_manifest(MAIN).unwrap(), ManifestAt::Absent);

    let m1 = files(&s, &[("README.md", b"one")]);
    let set1 = s.apply_set_files(MAIN, &m1, None).unwrap();
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m1));

    // Semantic ops on top inherit it — checked through the incrementally
    // extended snapshot after each op.
    let after_add = add(&s, "g", "g1");
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m1));
    let after_modify = modify(&s, "g", "g1", "g2");
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m1));
    assert_eq!(s.manifest_at(&after_add).unwrap(), set(&m1));
    assert_eq!(s.manifest_at(&after_modify).unwrap(), set(&m1));

    let m2 = files(&s, &[("README.md", b"two"), ("tests/t.lex", b"fn t() {}")]);
    s.apply_set_files(MAIN, &m2, None).unwrap();
    let tip = add(&s, "h", "h1");
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m2));
    assert_eq!(s.manifest_at(&tip).unwrap(), set(&m2));
    // History keeps its own answers.
    assert_eq!(s.manifest_at(&set1).unwrap(), set(&m1));
    assert_eq!(s.manifest_at(&after_modify).unwrap(), set(&m1));
    assert_eq!(s.manifest_at(&genesis).unwrap(), ManifestAt::Absent);

    // The cache agrees with a from-scratch computation, including a
    // snapshot written before #1007 (no `files` field).
    drop_snapshot(&s);
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m2));
    let snap = s
        .root()
        .join("branches")
        .join(format!("{MAIN}.head_snapshot.json"));
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert!(
        v.get("files").is_some(),
        "snapshot caches the manifest: {v}"
    );
    v.as_object_mut().unwrap().remove("files");
    std::fs::write(&snap, serde_json::to_vec(&v).unwrap()).unwrap();
    assert_eq!(s.branch_head(MAIN).unwrap().len(), 3);
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m2));
}

#[test]
fn missing_blob_is_rejected_and_the_head_is_unchanged() {
    let (s, _tmp) = fresh();
    add(&s, "f", "f1");
    let head_before = head(&s);
    let ops_before = op_count(&s);

    // An entry blob that was never uploaded.
    let absent = "a".repeat(64);
    let mut m = Manifest::new();
    m.entries.insert(
        "README.md".into(),
        Entry {
            blob: absent.clone(),
            mode: "100644".into(),
            size: 3,
        },
    );
    let id = s.put_manifest(&m).unwrap();
    match s.apply_set_files(MAIN, &id, None) {
        Err(StoreError::MissingBlobs(missing)) => assert_eq!(missing, vec![absent]),
        other => panic!("expected MissingBlobs, got {other:?}"),
    }
    // The manifest blob itself missing.
    let nowhere = "b".repeat(64);
    match s.apply_set_files(MAIN, &nowhere, None) {
        Err(StoreError::MissingBlobs(missing)) => assert_eq!(missing, vec![nowhere]),
        other => panic!("expected MissingBlobs, got {other:?}"),
    }
    assert_eq!(head(&s), head_before);
    assert_eq!(
        op_count(&s),
        ops_before,
        "a rejected SetFiles leaves no op behind"
    );
    assert_eq!(s.branch_manifest(MAIN).unwrap(), ManifestAt::Absent);

    // Negative control: with every blob present, a manifest lands.
    let ok = files(&s, &[("README.md", b"abc")]);
    s.apply_set_files(MAIN, &ok, None).unwrap();
    assert_ne!(head(&s), head_before);
}

#[test]
fn reserved_or_malformed_manifest_is_rejected_and_the_head_is_unchanged() {
    let (s, _tmp) = fresh();
    add(&s, "f", "f1");
    let head_before = head(&s);
    let ops_before = op_count(&s);

    // `put_manifest` validates, so write a hostile manifest as a raw blob —
    // exactly what a remote peer could upload.
    let blob = s.put_blob_bytes(b"fn main() -> Int { 1 }\n").unwrap();
    let mut m = Manifest::new();
    m.entries.insert(
        "src/main.lex".into(),
        Entry {
            blob: blob.clone(),
            mode: "100644".into(),
            size: 23,
        },
    );
    let reserved = s.put_blob_bytes(&m.to_canonical_bytes()).unwrap();
    assert!(matches!(
        s.apply_set_files(MAIN, &reserved, None),
        Err(StoreError::InvalidManifest(ManifestError::ReservedPath(p))) if p == "src/main.lex"
    ));

    // Not canonical (pretty-printed).
    let mut ok = Manifest::new();
    ok.entries.insert(
        "README.md".into(),
        Entry {
            blob: blob.clone(),
            mode: "100644".into(),
            size: 23,
        },
    );
    let pretty = s
        .put_blob_bytes(&serde_json::to_vec_pretty(&ok).unwrap())
        .unwrap();
    assert!(matches!(
        s.apply_set_files(MAIN, &pretty, None),
        Err(StoreError::InvalidManifest(ManifestError::NotCanonical))
    ));

    // Size that disagrees with the blob.
    let mut lying = Manifest::new();
    lying.entries.insert(
        "README.md".into(),
        Entry {
            blob,
            mode: "100644".into(),
            size: 1,
        },
    );
    let lying = s.put_blob_bytes(&lying.to_canonical_bytes()).unwrap();
    assert!(matches!(
        s.apply_set_files(MAIN, &lying, None),
        Err(StoreError::InvalidManifest(
            ManifestError::SizeMismatch { .. }
        ))
    ));

    assert_eq!(head(&s), head_before);
    assert_eq!(op_count(&s), ops_before);
    assert_eq!(s.branch_manifest(MAIN).unwrap(), ManifestAt::Absent);

    // Negative control: the well-formed one lands.
    let ok = s.put_manifest(&ok).unwrap();
    s.apply_set_files(MAIN, &ok, None).unwrap();
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&ok));
}

#[test]
fn replay_refuses_set_files_with_a_typed_reason_not_a_miss() {
    let (s, _tmp) = fresh();
    let src = "fn helper(x :: Int) -> Int { x }\n";
    let stage = lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap())
        .into_iter()
        .find(|st| matches!(st, lex_ast::Stage::FnDecl(_)))
        .unwrap();
    let sig = lex_ast::sig_id(&stage).unwrap();
    let stg = s.publish(&stage).unwrap();
    let add_op = add(&s, &sig, &stg);
    let m = files(&s, &[("README.md", b"x")]);
    let files_op = s.apply_set_files(MAIN, &m, None).unwrap();

    for result in [
        s.replay_request(&files_op).map(|_| ()),
        s.replay_record_miss(&files_op, "no output").map(|_| ()),
        s.replay_compare(&files_op, &stage).map(|_| ()),
    ] {
        match result {
            Err(StoreError::NotReplayable { op_id, why }) => {
                assert_eq!(op_id, files_op);
                assert_eq!(why, NotReplayable::Files);
            }
            other => panic!("expected NotReplayable::Files, got {other:?}"),
        }
    }
    // Refused, not recorded: no Replay attestation was emitted for it.
    let replays = s
        .attestation_log()
        .unwrap()
        .list_all()
        .unwrap()
        .into_iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::Replay { .. }))
        .count();
    assert_eq!(replays, 0);

    // Negative control: the semantic op beneath it is still replayable.
    let req = s.replay_request(&add_op).unwrap();
    assert_eq!(req.expected_stage_id, stg);
}

/// Land a record directly in the op log (the DAG shapes below — two
/// branches off one base — are about `manifest_at`, not branch plumbing).
fn put(log: &OpLog, op: Operation, produces: StageTransition) -> String {
    let rec = OperationRecord::new(op, produces);
    log.put(&rec).unwrap();
    rec.op_id
}

fn set_files_op(log: &OpLog, manifest: &str, parent: &str, intent: &str) -> String {
    let op = Operation::new(
        OperationKind::SetFiles {
            manifest: manifest.into(),
        },
        [parent.to_string()],
    )
    .with_intent(intent);
    put(log, op, StageTransition::FilesOnly)
}

/// A merge record with parents stored in exactly the given order — not
/// sorted — so the test can prove the answer ignores parent order.
fn merge_raw(log: &OpLog, parents: &[&str]) -> String {
    let op = Operation {
        kind: OperationKind::Merge { resolved: 0 },
        parents: parents.iter().map(|p| p.to_string()).collect(),
        intent_id: None,
    };
    put(
        log,
        op,
        StageTransition::Merge {
            entries: Default::default(),
        },
    )
}

#[test]
fn merge_with_disagreeing_parent_manifests_is_ambiguous_in_either_order() {
    let (s, _tmp) = fresh();
    let base = add(&s, "f", "f1");
    let log = OpLog::open(s.root()).unwrap();
    let m1 = files(&s, &[("README.md", b"left")]);
    let m2 = files(&s, &[("README.md", b"right")]);

    let left = set_files_op(&log, &m1, &base, "left");
    let right = set_files_op(&log, &m2, &base, "right");
    let plain = {
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "f".into(),
                from_stage_id: "f1".into(),
                to_stage_id: "f2".into(),
                from_budget: None,
                to_budget: None,
                to_sig_id: None,
            },
            [base.clone()],
        );
        put(
            &log,
            op,
            StageTransition::Replace {
                sig_id: "f".into(),
                from: "f1".into(),
                to: "f2".into(),
            },
        )
    };

    // Two different manifests; and a manifest vs no manifest at all. Each
    // pair in both stored orders — a "first parent wins" rule would pass one
    // order of each and fail the other.
    for (a, b) in [(&left, &right), (&left, &plain)] {
        for parents in [[a.as_str(), b.as_str()], [b.as_str(), a.as_str()]] {
            let m = merge_raw(&log, &parents);
            assert_eq!(
                s.manifest_at(&m).unwrap(),
                ManifestAt::Ambiguous,
                "parents {parents:?}"
            );
        }
    }

    // Negative control: parents that agree are not ambiguous.
    let left_again = set_files_op(&log, &m1, &base, "left-again");
    assert_ne!(left_again, left);
    let agree = merge_raw(&log, &[&left_again, &left]);
    assert_eq!(s.manifest_at(&agree).unwrap(), set(&m1));
    let neither = merge_raw(&log, &[&plain, &base]);
    assert_eq!(s.manifest_at(&neither).unwrap(), ManifestAt::Absent);

    // Ambiguity is sticky through semantic ops, and resolved by a SetFiles
    // recording the merged manifest.
    let merged = merge_raw(&log, &[&left, &right]);
    let on_top = {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "g".into(),
                stage_id: "g1".into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            [merged.clone()],
        );
        put(
            &log,
            op,
            StageTransition::Create {
                sig_id: "g".into(),
                stage_id: "g1".into(),
            },
        )
    };
    assert_eq!(s.manifest_at(&on_top).unwrap(), ManifestAt::Ambiguous);

    // Through a branch head too (cached path).
    s.advance_branch_head_ff(MAIN, &on_top).unwrap();
    assert_eq!(s.branch_manifest(MAIN).unwrap(), ManifestAt::Ambiguous);

    let m3 = files(&s, &[("README.md", b"left+right")]);
    let resolved = s.apply_set_files(MAIN, &m3, None).unwrap();
    assert_eq!(s.manifest_at(&resolved).unwrap(), set(&m3));
    assert_eq!(s.branch_manifest(MAIN).unwrap(), set(&m3));
}

#[test]
fn manifest_at_an_unknown_op_is_an_error() {
    let (s, _tmp) = fresh();
    assert!(matches!(
        s.manifest_at(&"0".repeat(64)),
        Err(StoreError::UnknownOp(_))
    ));
}
