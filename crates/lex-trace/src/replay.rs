//! Replay: re-execute a function with effect-output overrides keyed by NodeId.

use indexmap::IndexMap;

#[derive(Debug, Clone)]
pub struct Override {
    pub node_id: String,
    pub output: serde_json::Value,
}

/// Convert a list of overrides into the IndexMap shape the recorder uses.
pub fn replay_with_overrides(overrides: &[Override]) -> IndexMap<String, serde_json::Value> {
    let mut m = IndexMap::new();
    for o in overrides {
        m.insert(o.node_id.clone(), o.output.clone());
    }
    m
}
