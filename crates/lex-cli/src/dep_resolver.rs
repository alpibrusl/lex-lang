//! Client-side dependency resolver for the write-time publish gate (#930).
//!
//! When the loader stops inlining registry/git dependencies, a published head
//! keeps `import "<pkg>/<module>" as <alias>` edges and references
//! `<alias>.name`. The gate must then type-check that head against the
//! dependency's *signatures*. On the client (`lex publish`) those come from the
//! working copy: resolve each dependency's module file through the package
//! manifest and local package cache, load and type-check it, and return its
//! public function signatures as a module record.
//!
//! Leaf dependencies only: a dependency that itself has unresolved external
//! dependencies won't type-check standalone here and is skipped, leaving its
//! references unbound so the gate reports them rather than silently accepting.
//! (Recursive resolution of a dependency's own dependencies is a follow-up.)

use std::collections::BTreeMap;
use std::path::PathBuf;

use lex_store::DepResolver;
use lex_types::Ty;

pub(crate) struct ClientDepResolver {
    /// Any path inside the package being published; `resolve_package_import`
    /// finds the manifest and local cache relative to it.
    importer: PathBuf,
}

impl ClientDepResolver {
    pub(crate) fn new(importer: PathBuf) -> Self {
        Self { importer }
    }

    /// A dependency module's public surface: its function-signature record
    /// *and* its exported type declarations (#930 completeness). Both are read
    /// from the same loaded program — the value record feeds
    /// [`lex_store::DepResolver::resolve_modules`], the type decls feed
    /// [`lex_store::DepResolver::resolve_module_types`], so a dependent that
    /// references a dependency's *type* (not just its functions) type-checks.
    fn module_iface(&self, pkg: &str, module: &str) -> Option<(Ty, Vec<lex_ast::TypeDecl>)> {
        let module_file =
            lex_syntax::workspace::resolve_package_import(&self.importer, pkg, module).ok()?;
        // `load_program` inlines the dependency module's *local* closure and
        // keeps its own declarations under their bare names — exactly the
        // public surface a dependent reaches via `<alias>.name`.
        let prog = lex_syntax::load_program(&module_file).ok()?;
        let stages = lex_ast::canonicalize_program(&prog);
        let types = lex_types::check_program(&stages).ok()?;
        let fields = types
            .fn_signatures
            .iter()
            .map(|(name, scheme)| (name.clone(), scheme.ty.clone()));
        let record = lex_types::module_record_from_fields(fields);
        // Only the module's OWN types are part of its public surface. Types it
        // inlined from its *own* transitive dependencies carry a mangle prefix
        // (`constraints_<hash>.StrCheck`); a dependent that also imports that
        // transitive dependency directly reaches those types through its own
        // alias, so re-exposing the inlined copies here would register a second,
        // distinct qualified name (and duplicate constructors) for the same
        // type — a diamond mismatch. A module's own type names are bare (no
        // dot), so exclude the dotted, inlined ones.
        let type_decls: Vec<lex_ast::TypeDecl> = stages
            .into_iter()
            .filter_map(|s| match s {
                lex_ast::Stage::TypeDecl(td) if !td.name.contains('.') => Some(td),
                _ => None,
            })
            .collect();
        Some((record, type_decls))
    }
}

impl DepResolver for ClientDepResolver {
    fn resolve_modules(
        &self,
        stages: &[lex_ast::Stage],
        _head_op: Option<&str>,
    ) -> BTreeMap<String, Ty> {
        let mut out = BTreeMap::new();
        for st in stages {
            let lex_ast::Stage::Import(imp) = st else { continue };
            let Some((pkg, module)) = split_package_import(&imp.reference) else {
                continue;
            };
            if let Some((ty, _)) = self.module_iface(pkg, module) {
                out.insert(imp.reference.clone(), ty);
            }
        }
        out
    }

    fn resolve_module_types(
        &self,
        stages: &[lex_ast::Stage],
        _head_op: Option<&str>,
    ) -> BTreeMap<String, Vec<lex_ast::TypeDecl>> {
        let mut out = BTreeMap::new();
        for st in stages {
            let lex_ast::Stage::Import(imp) = st else { continue };
            let Some((pkg, module)) = split_package_import(&imp.reference) else {
                continue;
            };
            if let Some((_, decls)) = self.module_iface(pkg, module) {
                if !decls.is_empty() {
                    out.insert(imp.reference.clone(), decls);
                }
            }
        }
        out
    }
}

/// Classify `import "<ref>"`: `Some((pkg, module))` for a registry/git package
/// reference `<pkg>/<module>`, `None` for a local (`./`, `../`, `/`) or stdlib
/// (`std.*`) import. Mirrors the loader's own classification.
fn split_package_import(reference: &str) -> Option<(&str, &str)> {
    if reference.starts_with("./")
        || reference.starts_with("../")
        || reference.starts_with('/')
        || reference.starts_with("std.")
    {
        return None;
    }
    reference.split_once('/')
}

#[cfg(test)]
mod tests {
    use super::split_package_import;

    #[test]
    fn classifies_references() {
        assert_eq!(split_package_import("lex-nt/lib"), Some(("lex-nt", "lib")));
        assert_eq!(split_package_import("std.int"), None);
        assert_eq!(split_package_import("./local"), None);
        assert_eq!(split_package_import("../sib/mod"), None);
        assert_eq!(split_package_import("/abs/mod"), None);
        // A bare name with no `/` is not a package/module reference.
        assert_eq!(split_package_import("lonely"), None);
    }
}
