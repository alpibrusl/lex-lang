//! M2: canonical AST, node IDs, canonicalizer, canonical-JSON, hashing.
//!
//! See spec §5.

pub mod canonical;
pub mod canonicalize;
pub mod canon_json;
pub mod canon_print;
pub mod ids;

pub use canonical::*;
pub use canonicalize::{canonicalize_program, canonicalize_item};
pub use canon_print::print_stages;
pub use ids::{collect_ids, NodeId, NodeRef};

/// SHA-256 over the canonical-JSON encoding of a stage. Excludes NodeIds
/// (the canonical AST data does not carry IDs; they're derived).
pub fn stage_canonical_hash(stage: &Stage) -> [u8; 32] {
    let v = serde_json::to_value(stage).expect("stage is always serializable");
    canon_json::hash_canonical(&v)
}

pub fn stage_canonical_hash_hex(stage: &Stage) -> String {
    canon_json::hex(&stage_canonical_hash(stage))
}

/// SigId: §4.1. SHA-256 over canonical_json({name, input_types, output_type, effects}).
pub fn sig_id(stage: &Stage) -> Option<String> {
    let value = match stage {
        Stage::FnDecl(fd) => serde_json::json!({
            "effects": serde_json::to_value(&fd.effects).unwrap(),
            "input_types": serde_json::to_value(
                fd.params.iter().map(|p| &p.ty).collect::<Vec<_>>()
            ).unwrap(),
            "name": fd.name,
            "output_type": serde_json::to_value(&fd.return_type).unwrap(),
        }),
        Stage::TypeDecl(td) => serde_json::json!({
            "kind": "type",
            "name": td.name,
            "params": td.params,
        }),
        Stage::Import(_) => return None,
    };
    Some(canon_json::hex(&canon_json::hash_canonical(&value)))
}

/// StageId: §4.1. SHA-256 over (SigId || canonical_ast_hash). We don't yet
/// have implementation_metadata, so it's omitted.
pub fn stage_id(stage: &Stage) -> Option<String> {
    let sig = sig_id(stage)?;
    let ast_hash = stage_canonical_hash_hex(stage);
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sig.as_bytes());
    h.update(ast_hash.as_bytes());
    let r = h.finalize();
    Some(canon_json::hex(&r))
}
