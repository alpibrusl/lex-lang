//! A package's **public API** at an op, and how it changed between two ops —
//! the event source for the version-bump gate (#893) and, later, dependency
//! change-propagation.
//!
//! A package's public surface is its top-level functions and types (a Lex
//! `import "<pkg>/mod"` exposes the module's top-level names). Two versions'
//! surfaces are compared *structurally*: a function's signature is the JSON of
//! its `(param types, return type, effects)` — never its body or examples, so
//! a pure body change is a patch, not an API change. Names carry their
//! path-derived mangle prefix (`schema_a1b2.validate`), which is stable across
//! versions, so mangled names compare directly without de-mangling.

use std::collections::BTreeMap;

use crate::store::{Store, StoreError};

/// The public API at a head: declaration name → a structural signature string.
pub type PublicApi = BTreeMap<String, String>;

/// How a package's public API changed between two releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiChange {
    /// A public declaration was removed, or its signature changed — a
    /// consumer pinned to the old version can break. Requires a **major** bump.
    Breaking(String),
    /// Only additions (new public declarations); existing ones unchanged.
    /// Requires at least a **minor** bump.
    Additive(String),
    /// No public-API change (bodies may still differ). A **patch** suffices.
    None,
}

/// Extract the public API at `op_id`: every top-level fn/type declaration
/// keyed by name, valued by a structural signature that ignores body and
/// examples.
pub fn public_api_at_op(store: &Store, op_id: &str) -> Result<PublicApi, StoreError> {
    let head = crate::render::package_head_at_op(store, op_id)?;
    let pairs: Vec<(String, String)> =
        head.map.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let mut api = PublicApi::new();
    for ast in store.get_asts_for_sigs_bulk(&pairs) {
        match ast? {
            lex_ast::Stage::FnDecl(fd) => {
                // Signature = param types + return type + effects (JSON is a
                // stable, canonical structural key). Body and examples are
                // deliberately excluded: they don't affect type compatibility.
                let param_types: Vec<&lex_ast::TypeExpr> = fd.params.iter().map(|p| &p.ty).collect();
                let sig = serde_json::to_string(&(&param_types, &fd.return_type, &fd.effects))
                    .unwrap_or_default();
                api.insert(fd.name.clone(), format!("fn:{sig}"));
            }
            lex_ast::Stage::TypeDecl(td) => {
                let sig = serde_json::to_string(&td.definition).unwrap_or_default();
                api.insert(td.name.clone(), format!("type:{sig}"));
            }
            lex_ast::Stage::Import(_) => {}
        }
    }
    Ok(api)
}

/// Bare name for a message (strip the path-derived mangle prefix).
fn bare(name: &str) -> &str {
    name.split_once('.').map(|(_, n)| n).unwrap_or(name)
}

/// Classify the change from `prev` to `new`. Breaking dominates additive:
/// a release that both removes one name and adds another is Breaking.
pub fn classify_api_change(prev: &PublicApi, new: &PublicApi) -> ApiChange {
    for (name, sig) in prev {
        match new.get(name) {
            None => return ApiChange::Breaking(format!("`{}` was removed", bare(name))),
            Some(new_sig) if new_sig != sig => {
                return ApiChange::Breaking(format!("signature of `{}` changed", bare(name)))
            }
            _ => {}
        }
    }
    if let Some(added) = new.keys().find(|k| !prev.contains_key(*k)) {
        return ApiChange::Additive(format!("`{}` was added", bare(added)));
    }
    ApiChange::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(pairs: &[(&str, &str)]) -> PublicApi {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn removal_is_breaking() {
        let prev = api(&[("foo", "fn:A"), ("bar", "fn:B")]);
        let new = api(&[("foo", "fn:A")]);
        assert!(matches!(classify_api_change(&prev, &new), ApiChange::Breaking(_)));
    }

    #[test]
    fn signature_change_is_breaking() {
        let prev = api(&[("foo", "fn:A")]);
        let new = api(&[("foo", "fn:B")]);
        assert!(matches!(classify_api_change(&prev, &new), ApiChange::Breaking(_)));
    }

    #[test]
    fn pure_addition_is_additive() {
        let prev = api(&[("foo", "fn:A")]);
        let new = api(&[("foo", "fn:A"), ("bar", "fn:B")]);
        assert!(matches!(classify_api_change(&prev, &new), ApiChange::Additive(_)));
    }

    #[test]
    fn no_signature_change_is_none() {
        // Same signatures (a body-only change never reaches this map).
        let prev = api(&[("foo", "fn:A"), ("bar", "type:T")]);
        let new = api(&[("foo", "fn:A"), ("bar", "type:T")]);
        assert_eq!(classify_api_change(&prev, &new), ApiChange::None);
    }

    #[test]
    fn removal_plus_addition_is_breaking() {
        let prev = api(&[("foo", "fn:A")]);
        let new = api(&[("bar", "fn:B")]);
        assert!(matches!(classify_api_change(&prev, &new), ApiChange::Breaking(_)));
    }
}
