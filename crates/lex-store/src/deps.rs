//! Recursive, lock-driven dependency resolution for the write-time gate
//! (#943, #944).
//!
//! A non-inlined head keeps `import "<pkg>/<module>" as <alias>` edges (#930).
//! To type-check it, the gate needs each imported module's *surface*: its
//! function signatures, the type declarations those signatures mention, and
//! the module's mangle prefix. A dependency's surface in turn depends on
//! **its** dependencies, so resolution recurses:
//!
//! * each dependency's own dependencies resolve against **that dependency's**
//!   committed lock ([`Store::committed_lock_inherited`] at its pinned head),
//!   never the root's — a package is checked against exactly the pins it was
//!   published with;
//! * a `(store, head)` visited set turns a dependency cycle into a diagnostic
//!   instead of unbounded recursion, and a per-call cache resolves each
//!   `(store, head, module)` once however many paths reach it;
//! * the same package pinned to two different heads anywhere in the closure
//!   is a [`TypeError::DependencyConflict`] (a diamond whose sides disagree).
//!
//! Failures are **structured** ([`ResolvedDeps::diagnostics`]): an import the
//! lock does not pin is [`TypeError::UnpinnedDependency`] (#944 — hosted
//! resolution is registry-only; a git dependency has no lock entry), anything
//! else is [`TypeError::UnresolvedDependency`] with a reason. The gate reports
//! them instead of the downstream `unknown_identifier` they would otherwise
//! cause.
//!
//! What a pin *points at* is context-specific — the hub maps a registry URL to
//! a hosted store and enforces visibility — so it is abstracted as a
//! [`DepLocator`]; everything else lives here so every resolver agrees.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lex_syntax::lock::{LockEntry, LockFile};
use lex_types::{Ty, TypeError};

use crate::render::{map_stage_names, module_file, package_head_at_op, stage_prefix, NameSite};
use crate::store::{Store, StoreError};

/// Every dependency interface resolved for one head, keyed by import
/// reference (`"lex-nt/lib"`) — the three maps
/// [`lex_types::check_program_with_deps`] consumes, plus the diagnostics for
/// the imports that could not be resolved.
#[derive(Debug, Clone, Default)]
pub struct ResolvedDeps {
    /// import reference → that module's value record.
    pub modules: BTreeMap<String, Ty>,
    /// import reference → every type declaration the module's surface needs
    /// (its package's own, and — transitively — its dependencies'), all
    /// prefix-named when `prefixes` has an entry for the reference.
    pub types: BTreeMap<String, Vec<lex_ast::TypeDecl>>,
    /// import reference → the module's mangle prefix (prefixed mode, #963).
    pub prefixes: BTreeMap<String, String>,
    /// Structured reasons an import did not resolve. Non-empty means the head
    /// cannot be verified here.
    pub diagnostics: Vec<TypeError>,
}

/// One dependency module's public surface, as a dependent sees it.
#[derive(Debug, Clone)]
pub struct ModuleSurface {
    /// The module's functions keyed by bare name.
    pub record: Ty,
    /// Every type declaration the record can mention: the package's own and
    /// its dependencies' (transitively), dependencies first.
    pub types: Vec<lex_ast::TypeDecl>,
    /// The module's canonical mangle prefix, or `None` for a legacy head whose
    /// declarations carry no prefixes (its types are then bare-named and are
    /// registered under the importer's alias).
    pub prefix: Option<String>,
}

/// Classify `import "<ref>"`: `Some((pkg, module))` for a registry/git package
/// reference, `None` for a local (`./`, `../`, `/`) or stdlib (`std.*`) import.
/// Mirrors the loader's classification.
pub fn split_package_import(reference: &str) -> Option<(&str, &str)> {
    if reference.starts_with("./")
        || reference.starts_with("../")
        || reference.starts_with('/')
        || reference.starts_with("std.")
    {
        return None;
    }
    reference.split_once('/')
}

/// The standard hint for an import its lock does not pin (#944).
pub const UNPINNED_HINT: &str =
    "hosted verification resolves dependencies only through `lex.lock` \
pins into registry stores (it never fetches git): declare a registry source for it in `lex.toml` \
and run `lex pkg lock`";

/// Build the [`TypeError::UnpinnedDependency`] for `reference`.
pub fn unpinned(reference: &str, package: &str) -> TypeError {
    TypeError::UnpinnedDependency {
        at_node: "n_0".into(),
        reference: reference.to_string(),
        package: package.to_string(),
        hint: UNPINNED_HINT.to_string(),
    }
}

fn unresolved(reference: &str, package: &str, reason: impl Into<String>) -> TypeError {
    TypeError::UnresolvedDependency {
        at_node: "n_0".into(),
        reference: reference.to_string(),
        package: package.to_string(),
        reason: reason.into(),
    }
}

/// Where the store a lock entry pins lives, and whether the caller may read it.
pub trait DepLocator {
    /// Open the store `entry` pins. `Ok((identity, store))`, where `identity`
    /// names the store stably (it keys the cycle guard and the cache);
    /// `Err(reason)` when it is unreachable or not visible to the caller. The
    /// reason is shown to the user, so a store that exists but is private to
    /// someone else must read exactly like one that does not exist.
    fn open(&self, entry: &LockEntry) -> Result<(String, Store), String>;
}

/// The head's own import references (registry/git and otherwise), in order,
/// deduplicated.
fn import_refs(stages: &[lex_ast::Stage]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::Import(i) if seen.insert(i.reference.clone()) => {
                Some(i.reference.clone())
            }
            _ => None,
        })
        .collect()
}

/// Resolve every package import in `stages` against `lock` (the lock that
/// governs the head being gated), recursively. See the module docs.
pub fn resolve_with_lock(
    locator: &dyn DepLocator,
    stages: &[lex_ast::Stage],
    lock: Option<&LockFile>,
) -> ResolvedDeps {
    let mut walk = Walk {
        locator,
        surfaces: HashMap::new(),
        deps: HashMap::new(),
        visiting: HashSet::new(),
        pins: BTreeMap::new(),
    };
    let mut out = walk.resolve_refs(&import_refs(stages), lock);
    // Diamond check across the whole closure the walk saw.
    for (package, heads) in &walk.pins {
        if heads.len() > 1 {
            out.diagnostics.push(TypeError::DependencyConflict {
                at_node: "n_0".into(),
                package: package.clone(),
                heads: heads.iter().cloned().collect(),
            });
        }
    }
    out
}

/// Parse a committed lock, treating a malformed one as absent (every import
/// then reports as unpinned, which is what it effectively is).
pub fn parse_lock(toml: &str) -> Option<LockFile> {
    LockFile::from_toml(toml).ok()
}

struct Walk<'a> {
    locator: &'a dyn DepLocator,
    /// (store identity, head, module) → surface or failure reason.
    surfaces: HashMap<(String, String, String), Result<ModuleSurface, String>>,
    /// (store identity, head) → that head's own resolved dependencies.
    deps: HashMap<(String, String), ResolvedDeps>,
    /// (store identity, head) pairs on the current resolution path.
    visiting: HashSet<(String, String)>,
    /// package name → every head it was pinned to anywhere in the closure.
    pins: BTreeMap<String, BTreeSet<String>>,
}

impl Walk<'_> {
    fn resolve_refs(&mut self, refs: &[String], lock: Option<&LockFile>) -> ResolvedDeps {
        let mut out = ResolvedDeps::default();
        for reference in refs {
            let Some((pkg, module)) = split_package_import(reference) else {
                continue;
            };
            let Some(entry) = lock.and_then(|l| l.entry(pkg)) else {
                out.diagnostics.push(unpinned(reference, pkg));
                continue;
            };
            let Some(head) = entry.head_op.clone() else {
                out.diagnostics.push(unresolved(
                    reference,
                    pkg,
                    format!(
                        "the lock pins version {} but no op-log head; re-run `lex pkg lock` against a registry that serves heads",
                        entry.version
                    ),
                ));
                continue;
            };
            self.pins
                .entry(pkg.to_string())
                .or_default()
                .insert(head.clone());
            match self.surface(entry, &head, pkg, module) {
                Ok(s) => {
                    out.modules.insert(reference.clone(), s.record);
                    if !s.types.is_empty() {
                        out.types.insert(reference.clone(), s.types);
                    }
                    if let Some(p) = s.prefix {
                        out.prefixes.insert(reference.clone(), p);
                    }
                }
                Err(reason) => out.diagnostics.push(unresolved(reference, pkg, reason)),
            }
        }
        out
    }

    fn surface(
        &mut self,
        entry: &LockEntry,
        head: &str,
        pkg: &str,
        module: &str,
    ) -> Result<ModuleSurface, String> {
        let (id, store) = self.locator.open(entry)?;
        let key = (id.clone(), head.to_string(), module.to_string());
        if let Some(hit) = self.surfaces.get(&key) {
            return hit.clone();
        }
        let node = (id.clone(), head.to_string());
        if self.visiting.contains(&node) {
            // Not cached: whether this is a cycle depends on the path taken.
            return Err(format!(
                "dependency cycle: `{pkg}` at {head} depends on itself"
            ));
        }
        let deps = match self.deps.get(&node) {
            Some(d) => d.clone(),
            None => {
                self.visiting.insert(node.clone());
                let d = self.deps_of(&store, head);
                self.visiting.remove(&node);
                self.deps.insert(node.clone(), d.clone());
                d
            }
        };
        let result = if deps.diagnostics.is_empty() {
            module_surface_at_op_with(&store, head, pkg, module, &deps).map_err(|e| e.to_string())
        } else {
            let inner: Vec<String> = deps.diagnostics.iter().map(|d| d.to_string()).collect();
            Err(format!(
                "its own dependencies do not resolve: {}",
                inner.join("; ")
            ))
        };
        self.surfaces.insert(key, result.clone());
        result
    }

    /// A dependency head's own dependencies, resolved against **its** lock.
    fn deps_of(&mut self, store: &Store, head: &str) -> ResolvedDeps {
        let refs: Vec<String> = match package_head_at_op(store, head) {
            Ok(h) => h.flat_imports.keys().cloned().collect(),
            Err(e) => {
                let mut d = ResolvedDeps::default();
                d.diagnostics
                    .push(unresolved("", "", format!("reading head {head}: {e}")));
                return d;
            }
        };
        if !refs.iter().any(|r| split_package_import(r).is_some()) {
            return ResolvedDeps::default();
        }
        let lock = store
            .committed_lock_inherited(head)
            .ok()
            .flatten()
            .and_then(|t| parse_lock(&t));
        self.resolve_refs(&refs, lock.as_ref())
    }
}

/// The surface of `module` in the package head `head_op` of `store` (the
/// package named `package`), type-checked against its own already-resolved
/// dependencies `deps` (#943).
///
/// A package published whole (`lex publish <dir>`) carries per-file mangle
/// prefixes. Those are **re-prefixed by package identity** —
/// [`lex_syntax::package_file_prefix`]`(package, file)`, the prefix a client
/// that loads the dependency from source gives it — so two packages that
/// both ship `src/error.lex` never share an `error_<hash>` namespace, whatever
/// prefix their op-logs were published under. References to the package's own
/// dependencies (`c.Thing`) are rewritten in type positions to those
/// dependencies' canonical prefixes, so the exported type declarations are
/// meaningful in any importer's scope.
///
/// A legacy head with no prefixes (a single-file publish) resolves in bare
/// mode: bare-named record and types, no prefix.
pub fn module_surface_at_op_with(
    store: &Store,
    head_op: &str,
    package: &str,
    module: &str,
    deps: &ResolvedDeps,
) -> Result<ModuleSurface, StoreError> {
    let head = package_head_at_op(store, head_op)?;
    let pairs: Vec<(String, String)> = head
        .map
        .iter()
        .map(|(s, st)| (s.clone(), st.clone()))
        .collect();
    let mut decls: Vec<(Option<String>, lex_ast::Stage)> = Vec::new();
    for ((sig, _), ast) in pairs.iter().zip(store.get_asts_for_sigs_bulk(&pairs)) {
        decls.push((head.sig_files.get(sig).cloned(), ast?));
    }

    // import alias → canonical prefix of the dependency module it names.
    let alias_prefix: BTreeMap<String, String> = head
        .flat_imports
        .iter()
        .filter_map(|(r, alias)| deps.prefixes.get(r).map(|p| (alias.clone(), p.clone())))
        .collect();
    let dep_types: Vec<lex_ast::TypeDecl> = {
        let mut seen = BTreeSet::new();
        deps.types
            .values()
            .flatten()
            .filter(|d| seen.insert(d.name.clone()))
            .cloned()
            .collect()
    };

    let prefixed = !decls.is_empty()
        && decls
            .iter()
            .all(|(file, s)| file.is_some() && stage_prefix(s).is_some());

    // stored prefix → canonical prefix, and file → canonical prefix.
    let mut rename: BTreeMap<String, String> = BTreeMap::new();
    let mut file_prefix: BTreeMap<String, String> = BTreeMap::new();
    if prefixed {
        for (file, s) in &decls {
            let (Some(file), Some(stored)) = (file, stage_prefix(s)) else {
                continue;
            };
            let canonical = lex_syntax::package_file_prefix(package, file);
            rename.insert(stored, canonical.clone());
            file_prefix.insert(file.clone(), canonical);
        }
    }

    let mut stages: Vec<lex_ast::Stage> = head
        .flat_imports
        .iter()
        .map(|(reference, alias)| {
            lex_ast::Stage::Import(lex_ast::Import {
                reference: reference.clone(),
                alias: alias.clone(),
            })
        })
        .collect();
    let body: Vec<lex_ast::Stage> = if prefixed {
        decls
            .into_iter()
            .map(|(_, mut s)| {
                map_stage_names(&mut s, &mut |site, name| {
                    let Some((q, rest)) = name.split_once('.') else {
                        return name.to_string();
                    };
                    if let Some(c) = rename.get(q) {
                        return format!("{c}.{rest}");
                    }
                    match (site, alias_prefix.get(q)) {
                        (NameSite::Type, Some(p)) => format!("{p}.{rest}"),
                        _ => name.to_string(),
                    }
                });
                s
            })
            .collect()
    } else {
        // Bare mode: de-mangle exactly as the pre-#943 single-module path did.
        let mut v = match crate::render::demangled_module_stages(store, head_op, module) {
            Ok(v) => v,
            Err(_) => crate::render::demangled_head_stages(store, head_op)?,
        };
        v.retain(|s| !matches!(s, lex_ast::Stage::Import(_)));
        for s in &mut v {
            map_stage_names(s, &mut |site, name| match (site, name.split_once('.')) {
                (NameSite::Type, Some((q, rest))) => match alias_prefix.get(q) {
                    Some(p) => format!("{p}.{rest}"),
                    None => name.to_string(),
                },
                _ => name.to_string(),
            });
        }
        v
    };
    stages.extend(body.iter().cloned());

    let types =
        lex_types::check_program_with_deps(&stages, &deps.modules, &deps.types, &deps.prefixes)
            .map_err(StoreError::TypeError)?;

    let own_types = body.iter().filter_map(|s| match s {
        lex_ast::Stage::TypeDecl(td) => Some(td.clone()),
        _ => None,
    });

    if prefixed {
        let target = module_file(file_prefix.keys(), module)
            .or_else(|| {
                (file_prefix.len() == 1)
                    .then(|| file_prefix.keys().next().cloned())
                    .flatten()
            })
            .ok_or(StoreError::UnsupportedMultiModuleDependency)?;
        let prefix = file_prefix[&target].clone();
        let dot = format!("{prefix}.");
        let record = lex_types::module_record_from_fields(types.fn_signatures.iter().filter_map(
            |(name, scheme)| {
                name.strip_prefix(&dot)
                    .map(|bare| (bare.to_string(), scheme.ty.clone()))
            },
        ));
        let mut all = dep_types;
        all.extend(own_types);
        Ok(ModuleSurface {
            record,
            types: all,
            prefix: Some(prefix),
        })
    } else {
        let record = lex_types::module_record_from_fields(
            types
                .fn_signatures
                .iter()
                .filter(|(name, _)| !name.contains('.'))
                .map(|(name, scheme)| (name.clone(), scheme.ty.clone())),
        );
        // Bare mode registers `types` under the importer's alias, so only the
        // package's own bare-named declarations can travel this way.
        let own: Vec<lex_ast::TypeDecl> = own_types.filter(|t| !t.name.contains('.')).collect();
        Ok(ModuleSurface {
            record,
            types: own,
            prefix: None,
        })
    }
}
