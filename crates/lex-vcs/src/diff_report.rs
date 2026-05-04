//! Plain-data shape of the `lex ast-diff` output. Lives in lex-vcs
//! so both the CLI (which produces it) and `diff_to_ops` (which
//! consumes it) can share types without a cyclic dep.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AddRemove {
    pub name: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    pub from: String,
    pub to: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Modified {
    pub name: String,
    pub signature_before: String,
    pub signature_after: String,
    pub signature_changed: bool,
    pub effect_changes: EffectChanges,
    pub body_patches: Vec<BodyPatch>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EffectChanges {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BodyPatch {
    pub op: String,
    pub node_path: String,
    pub from_kind: String,
    pub to_kind: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiffReport {
    pub added: Vec<AddRemove>,
    pub removed: Vec<AddRemove>,
    pub renamed: Vec<Renamed>,
    pub modified: Vec<Modified>,
}
