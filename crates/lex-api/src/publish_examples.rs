//! #835 Tier 1: record the behavioral-examples verdict for a publish.
//! Extracted from `handlers.rs` to keep that file within the source line
//! budget; the behavioral run happens in `publish_handler`, this only
//! records the per-stage `Examples::Passed` attestation.

/// Emit `Examples::Passed` for each published fn-stage that declares
/// examples (#835). The examples were behaviorally run and passed in
/// `publish_handler` before the store write; this records the verdict so
/// the opt-in `required_attestations: [Examples]` gate has something to
/// match. Best-effort: recording failures are logged, never fatal.
pub(crate) fn record_examples_for_publish(
    store: &lex_store::Store,
    stages: &[lex_ast::Stage],
    outcome: &lex_store::PublishOutcome,
) {
    use std::collections::BTreeMap;
    // stage_id -> op_id, from the ops this publish produced.
    let op_for_stage: BTreeMap<String, String> = outcome
        .ops
        .iter()
        .filter_map(|op| {
            op.kind.get("stage_id").and_then(|v| v.as_str())
                .map(|sid| (sid.to_string(), op.op_id.clone()))
        })
        .collect();
    for stage in stages {
        let lex_ast::Stage::FnDecl(fd) = stage else { continue };
        if fd.examples.is_empty() {
            continue;
        }
        let Some(sid) = lex_ast::stage_id(stage) else { continue };
        // The op that produced this stage; fall back to head_op for a
        // stage that was already at head (republish no-op).
        let op_id = op_for_stage.get(&sid).cloned()
            .or_else(|| outcome.head_op.clone());
        if let Some(op_id) = op_id {
            if let Err(e) = store.record_examples_passed(&sid, &op_id, fd.examples.len()) {
                eprintln!("warn: record_examples_passed({sid}) failed: {e}");
            }
        }
    }
}

