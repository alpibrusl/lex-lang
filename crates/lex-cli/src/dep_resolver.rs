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
    /// A dependency module's public surface, resolved with the whole
    /// dependency loaded as **one package** (#963). `load_package` gives every
    /// file in the dependency a deterministic, path-derived mangle prefix
    /// (`connection_<hash>`, `error_<hash>`), so a module imported directly and
    /// the copies of it inlined into its sibling modules share ONE identity —
    /// the diamond `load_program`-per-module produced (a bare direct import vs
    /// an `error_<hash>` inlined copy) disappears. Returns:
    /// - the value record, this module's own functions keyed by bare name
    ///   (prefix stripped), signatures kept as loaded (cross-module references
    ///   stay prefix-qualified);
    /// - every type declaration in the loaded package (prefix-named, globally
    ///   unique), so any type the module's surface exposes resolves;
    /// - this module's own mangle prefix, so the checker can map the import
    ///   alias to it (`<alias>.Type` → `<prefix>.Type`).
    fn module_iface(
        &self,
        pkg: &str,
        module: &str,
    ) -> Option<(Ty, Vec<lex_ast::TypeDecl>, String)> {
        let module_file =
            lex_syntax::workspace::resolve_package_import(&self.importer, pkg, module).ok()?;
        let (_toml, pkg_root) = lex_syntax::workspace::find_manifest(&module_file)?;
        let loaded = lex_syntax::load_package(
            std::slice::from_ref(&module_file),
            &pkg_root,
            pkg,
            /*inline_packages=*/ true,
        )
        .ok()?;
        let stages = lex_ast::canonicalize_program(&loaded.program);
        let types = lex_types::check_program(&stages).ok()?;

        // This module's own prefix: the `module_prefixes` entry whose file is
        // this module, relative to the (canonicalized) package root.
        let root_c = pkg_root.canonicalize().ok()?;
        let module_rel = module_file
            .canonicalize()
            .ok()?
            .strip_prefix(&root_c)
            .ok()?
            .to_string_lossy()
            .to_string();
        let prefix = loaded
            .module_prefixes
            .iter()
            .find(|(_p, f)| f.as_str() == module_rel)
            .map(|(p, _)| p.clone())?;

        // Value record: this module's own functions, bare-keyed.
        let dot = format!("{prefix}.");
        let fields = types.fn_signatures.iter().filter_map(|(name, scheme)| {
            name.strip_prefix(&dot).map(|bare| (bare.to_string(), scheme.ty.clone()))
        });
        let record = lex_types::module_record_from_fields(fields);

        // Every type in the loaded package — prefix-named and globally unique,
        // so the checker registers them as-is and any exposed type resolves.
        let type_decls: Vec<lex_ast::TypeDecl> = stages
            .into_iter()
            .filter_map(|s| match s {
                lex_ast::Stage::TypeDecl(td) => Some(td),
                _ => None,
            })
            .collect();
        Some((record, type_decls, prefix))
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
            if let Some((ty, _, _)) = self.module_iface(pkg, module) {
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
            if let Some((_, decls, _)) = self.module_iface(pkg, module) {
                if !decls.is_empty() {
                    out.insert(imp.reference.clone(), decls);
                }
            }
        }
        out
    }

    fn resolve_module_prefixes(
        &self,
        stages: &[lex_ast::Stage],
        _head_op: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for st in stages {
            let lex_ast::Stage::Import(imp) = st else { continue };
            let Some((pkg, module)) = split_package_import(&imp.reference) else {
                continue;
            };
            if let Some((_, _, prefix)) = self.module_iface(pkg, module) {
                out.insert(imp.reference.clone(), prefix);
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
