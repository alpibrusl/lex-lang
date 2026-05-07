//! M2: canonical AST, node IDs, canonicalizer, canonical-JSON, hashing.
//!
//! See spec §5.

pub mod canonical;
pub mod canonicalize;
pub mod canon_json;
pub mod canon_print;
pub mod canonical_format;
pub mod dead_branch;
pub mod ids;
pub mod patch;

pub use canonical::*;
pub use canonicalize::{canonicalize_program, canonicalize_item};
pub use canon_print::print_stages;
pub use ids::{collect_ids, expr_ids, NodeId, NodeRef};
pub use patch::{apply_patch, Patch, PatchError};

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
    Some(canon_json::hex(&sig_hash(stage, true)?))
}

/// Structural-sig hash: like SigId but with the name omitted. Used as
/// input to StageId so renames don't change implementation identity
/// (per §4.6's open-question default: name lives in SigId, not StageId).
fn structural_sig_hash(stage: &Stage) -> Option<[u8; 32]> {
    sig_hash(stage, false)
}

fn sig_hash(stage: &Stage, include_name: bool) -> Option<[u8; 32]> {
    let value = match stage {
        Stage::FnDecl(fd) => {
            let mut v = serde_json::Map::new();
            v.insert("effects".into(), serde_json::to_value(&fd.effects).unwrap());
            v.insert("input_types".into(), serde_json::to_value(
                fd.params.iter().map(|p| &p.ty).collect::<Vec<_>>()
            ).unwrap());
            v.insert("output_type".into(), serde_json::to_value(&fd.return_type).unwrap());
            if include_name { v.insert("name".into(), serde_json::Value::String(fd.name.clone())); }
            serde_json::Value::Object(v)
        }
        Stage::TypeDecl(td) => {
            let mut v = serde_json::Map::new();
            v.insert("kind".into(), serde_json::Value::String("type".into()));
            v.insert("params".into(), serde_json::to_value(&td.params).unwrap());
            if include_name { v.insert("name".into(), serde_json::Value::String(td.name.clone())); }
            serde_json::Value::Object(v)
        }
        Stage::Import(_) => return None,
    };
    Some(canon_json::hash_canonical(&value))
}

/// StageId: §4.1, with §4.6 default applied.
///
/// `StageId = SHA-256(structural_sig_hash || implementation_hash)`.
///
/// `structural_sig_hash` is SigId with the name field omitted, and
/// `implementation_hash` is the canonical AST with the name field blanked.
/// Together they encode "what the function does and what shape it has";
/// the name lives in SigId only.
pub fn stage_id(stage: &Stage) -> Option<String> {
    let sig = canon_json::hex(&structural_sig_hash(stage)?);
    let impl_h = canon_json::hex(&implementation_hash(stage));
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sig.as_bytes());
    h.update(impl_h.as_bytes());
    let r = h.finalize();
    Some(canon_json::hex(&r))
}

/// SHA-256 over the canonical AST with the *name* fields blanked out.
/// Two implementations that differ only in their function/type name
/// produce the same `implementation_hash` (and therefore the same StageId,
/// given matching SigIds).
pub fn implementation_hash(stage: &Stage) -> [u8; 32] {
    let stripped = strip_names(stage);
    let v = serde_json::to_value(&stripped).expect("stage is always serializable");
    canon_json::hash_canonical(&v)
}

fn strip_names(stage: &Stage) -> Stage {
    match stage.clone() {
        Stage::FnDecl(mut fd) => { fd.name = String::new(); Stage::FnDecl(fd) }
        Stage::TypeDecl(mut td) => { td.name = String::new(); Stage::TypeDecl(td) }
        s @ Stage::Import(_) => s,
    }
}
