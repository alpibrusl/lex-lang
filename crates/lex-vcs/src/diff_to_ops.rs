//! Convert a `DiffReport` (+ import set deltas + old head info)
//! into a sequence of typed operations.
//!
//! NOTE: `lex-cli`'s `compute_diff` (the only producer of `DiffReport`
//! today) only diffs `Stage::FnDecl` — types are not yet surfaced.
//! The `RemoveType`, `AddType`, and `ModifyType` branches below are
//! forward-looking placeholders that will activate when type-decl
//! diffing lands. The fn-vs-type heuristic uses
//! `signature.starts_with("type ")` which depends on the renderer
//! in `lex-cli/src/diff.rs::render_signature` for `TypeDecl` to
//! produce strings beginning with "type ". When types come online,
//! consider extending `AddRemove` with a `kind: SymbolKind` field
//! to make this typed rather than string-prefix-based.

use crate::diff_report::DiffReport;
use crate::operation::{default_import_alias, EffectSet, OperationKind, SigId, StageId};
use lex_ast::{sig_id, stage_id, Effect, Stage};
use std::collections::{BTreeMap, BTreeSet};

/// One import as the diff sees it: the module reference plus the alias
/// it is bound under. The alias is part of the set key, so re-aliasing
/// an already-imported module (`as sql` → `as db`) reads as a
/// remove + add, which is exactly the pair of ops that reproduces it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ImportRef {
    pub reference: String,
    pub alias: String,
}

pub type ImportMap = BTreeMap<String, BTreeSet<ImportRef>>;

#[derive(Debug, thiserror::Error)]
pub enum DiffMappingError {
    #[error("diff mentions removed/modified name `{0}` but no old_sig_id was resolved for it")]
    MissingOldSigForName(String),
    #[error("diff mentions added/renamed name `{0}` but new_stages has no matching stage")]
    MissingNewStageForName(String),
    #[error("sig `{0}` is in the diff's old_sig_id but not in old_head")]
    MissingOldHeadForSig(SigId),
    #[error("stage for `{0}` produces no sig_id (likely an Import that slipped through)")]
    NoSigIdForStage(String),
    #[error("stage for `{0}` produces no stage_id (likely an Import that slipped through)")]
    NoStageIdForStage(String),
}

#[derive(Debug)]
pub struct DiffInputs<'a> {
    /// Current head SigId → StageId map.
    pub old_head: &'a BTreeMap<SigId, StageId>,
    /// Effect set per sig at the current head.
    pub old_effects: &'a BTreeMap<SigId, EffectSet>,
    /// Per-file imports at the current head.
    pub old_imports: &'a ImportMap,
    /// Stages of the new program (post-canonicalize).
    pub new_stages: &'a [Stage],
    /// Per-file imports of the new program.
    pub new_imports: &'a ImportMap,
    /// AST-diff between old and new sources, by name. Each removed/
    /// renamed/modified entry carries its own resolved `old_sig_id`
    /// (see `diff_report`'s doc comments) rather than this module
    /// re-deriving one from a name-keyed lookup: a bare function name
    /// is not unique across a package's files (#818 — e.g. two files
    /// legitimately both declaring a local `validate` helper with
    /// different signatures), so a `name -> SigId` map here would
    /// silently collapse distinct functions onto one SigId.
    pub diff: &'a DiffReport,
    /// Mangling prefix → package source file (`schema_a1b2` →
    /// `src/schema.lex`), for a multi-module package publish. A
    /// declaration's mangled name is `<prefix>.<local>`, so this maps
    /// each `AddFunction`/`AddType` to its origin file, recorded as the
    /// op's `in_file` so `export-git` can de-flatten the package (#894).
    /// Empty for a single-file publish (then `in_file` stays `None` and
    /// OpIds are unchanged).
    pub module_prefixes: &'a BTreeMap<String, String>,
}

pub fn diff_to_ops(inputs: DiffInputs<'_>) -> Result<Vec<OperationKind>, DiffMappingError> {
    let mut out = Vec::new();
    let new_by_name: BTreeMap<&str, &Stage> = inputs.new_stages.iter()
        .filter_map(|s| {
            let n = match s {
                Stage::FnDecl(fd) => fd.name.as_str(),
                Stage::TypeDecl(td) => td.name.as_str(),
                Stage::Import(_) => return None,
            };
            Some((n, s))
        })
        .collect();

    // 1. Imports — separate from stage ops; emit first so importer
    //    state is consistent before any sig ops apply.
    for (file, modules) in inputs.new_imports {
        let old = inputs.old_imports.get(file).cloned().unwrap_or_default();
        // Removes BEFORE adds (#909). A re-alias (`as e` → `as err`) is a
        // remove + add of the SAME module reference, and a `RemoveImport` is
        // keyed by reference alone (it carries no alias) — so replaying
        // add-then-remove leaves the import gone rather than re-aliased, and
        // `export-git` falls back to a derived alias. Remove-then-add leaves it
        // bound to the new alias, which is what the pair means.
        for m in old.difference(modules) {
            out.push(OperationKind::RemoveImport {
                in_file: file.clone(),
                module: m.reference.clone(),
            });
        }
        for m in modules.difference(&old) {
            out.push(OperationKind::AddImport {
                in_file: file.clone(),
                module: m.reference.clone(),
                alias: explicit_alias(m),
            });
        }
    }
    for (file, old) in inputs.old_imports {
        if !inputs.new_imports.contains_key(file) {
            for m in old {
                out.push(OperationKind::RemoveImport {
                    in_file: file.clone(),
                    module: m.reference.clone(),
                });
            }
        }
    }

    // 2. Removed → RemoveFunction / RemoveType.
    for r in &inputs.diff.removed {
        let Some(sig) = r.old_sig_id.as_ref() else {
            return Err(DiffMappingError::MissingOldSigForName(r.name.clone()));
        };
        let Some(last) = inputs.old_head.get(sig) else {
            return Err(DiffMappingError::MissingOldHeadForSig(sig.clone()));
        };
        // Decide fn vs type by looking at the diff signature string:
        // type signatures start with "type ".
        if r.signature.starts_with("type ") {
            out.push(OperationKind::RemoveType {
                sig_id: sig.clone(),
                last_stage_id: last.clone(),
            });
        } else {
            out.push(OperationKind::RemoveFunction {
                sig_id: sig.clone(),
                last_stage_id: last.clone(),
            });
        }
    }

    // 3. Added → AddFunction / AddType.
    for a in &inputs.diff.added {
        let Some(stage) = new_by_name.get(a.name.as_str()) else {
            return Err(DiffMappingError::MissingNewStageForName(a.name.clone()));
        };
        let Some(sig) = sig_id(stage) else {
            return Err(DiffMappingError::NoSigIdForStage(a.name.clone()));
        };
        let Some(stg) = stage_id(stage) else {
            return Err(DiffMappingError::NoStageIdForStage(a.name.clone()));
        };
        let in_file = origin_file(&a.name, inputs.module_prefixes);
        match stage {
            Stage::FnDecl(fd) => {
                let effects = effect_set(&fd.effects);
                // #247: extract the function's declared `[budget(N)]`
                // from its effect set so the op log carries the
                // initial cost without rehydrating the stage at
                // query time.
                let budget_cost = crate::operation::budget_from_effects(&effects);
                out.push(OperationKind::AddFunction {
                    sig_id: sig, stage_id: stg, effects, budget_cost, in_file,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::AddType { sig_id: sig, stage_id: stg, in_file });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    // 4. Renamed → RenameSymbol.
    for r in &inputs.diff.renamed {
        let from_sig = &r.old_sig_id;
        let Some(stage) = new_by_name.get(r.to.as_str()) else {
            return Err(DiffMappingError::MissingNewStageForName(r.to.clone()));
        };
        let Some(to_sig) = sig_id(stage) else {
            return Err(DiffMappingError::NoSigIdForStage(r.to.clone()));
        };
        let Some(body_id) = stage_id(stage) else {
            return Err(DiffMappingError::NoStageIdForStage(r.to.clone()));
        };
        out.push(OperationKind::RenameSymbol {
            from: from_sig.clone(),
            to: to_sig,
            body_stage_id: body_id,
        });
    }

    // 5. Modified → ChangeEffectSig | ModifyBody | ModifyType.
    for m in &inputs.diff.modified {
        let sig = &m.old_sig_id;
        let Some(from_id) = inputs.old_head.get(sig) else {
            return Err(DiffMappingError::MissingOldHeadForSig(sig.clone()));
        };
        let Some(stage) = new_by_name.get(m.name.as_str()) else {
            return Err(DiffMappingError::MissingNewStageForName(m.name.clone()));
        };
        let Some(to_id) = stage_id(stage) else {
            return Err(DiffMappingError::NoStageIdForStage(m.name.clone()));
        };
        let effects_changed =
            !m.effect_changes.added.is_empty() || !m.effect_changes.removed.is_empty();
        // #992: record the sig the declaration moves *to* whenever the new
        // stage hashes to a different one. A SigId covers the effect row, the
        // input/output types, the signature-level examples and (for a type)
        // its params — so any of those changing under the same name is a sig
        // move, and leaving the head bound to the old sig produces an entry no
        // store can ever satisfy. `None` for a body-only change.
        let to_sig_id = sig_id(stage).filter(|s| s != sig);
        match stage {
            Stage::FnDecl(fd) if effects_changed => {
                let from_effects = inputs.old_effects.get(sig).cloned().unwrap_or_default();
                let to_effects = effect_set(&fd.effects);
                // #247: the budget delta. ChangeEffectSig fires
                // because the effect set changed, which often
                // includes the `[budget(N)]` declaration itself.
                let from_budget = crate::operation::budget_from_effects(&from_effects);
                let to_budget = crate::operation::budget_from_effects(&to_effects);
                out.push(OperationKind::ChangeEffectSig {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                    from_effects,
                    to_effects,
                    from_budget,
                    to_budget,
                    to_sig_id,
                });
            }
            Stage::FnDecl(fd) => {
                // #247: ModifyBody fires when only the body changed
                // — effects (including budget) are unchanged. Pull
                // the budget from the new stage's effect set; the
                // old effect set in `old_effects[sig]` would yield
                // the same value.
                let to_effects = effect_set(&fd.effects);
                let budget = crate::operation::budget_from_effects(&to_effects);
                out.push(OperationKind::ModifyBody {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                    from_budget: budget,
                    to_budget: budget,
                    to_sig_id,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::ModifyType {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                    to_sig_id,
                });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    Ok(out)
}

/// Project a slice of effects into the canonical `EffectSet` (sorted
/// label strings).
///
/// Effect args are preserved via the canonical pretty-print form
/// (e.g. `fs_read("/tmp")`, `net("wttr.in")`) — see
/// `compute_diff::effect_label`. This makes `[net]` → `[net("wttr.in")]`
/// a real `ChangeEffectSig` op (the strings differ), satisfying #207's
/// third acceptance criterion via #223.
///
/// **OpId stability**: bare effects still produce just `"net"` (not
/// `"net()"` or any other suffix), so every pre-#223 op log retains
/// its existing OpIds. Only ops *introducing* parameterized effects
/// see new hashes — and those are by definition new ops.
fn effect_set(effs: &[Effect]) -> EffectSet {
    effs.iter().map(crate::compute_diff::effect_label).collect()
}

/// The package source file a declaration came from, or `None`. A
/// package-mangled name is `<prefix>.<local>` (e.g.
/// `schema_a1b2.validate`); `module_prefixes` maps the prefix to its
/// file. A single-file publish leaves names unmangled and passes an
/// empty map, so this is `None` and the op's `in_file` is omitted.
fn origin_file(name: &str, module_prefixes: &BTreeMap<String, String>) -> Option<String> {
    let prefix = name.split_once('.')?.0;
    module_prefixes.get(prefix).cloned()
}

/// The alias to record on an `AddImport`, or `None` when it is just the
/// module's default alias. Omitting the default keeps the common-case
/// `AddImport` byte-identical to its pre-alias form, so its `OpId` does
/// not rotate; only a deliberately-renamed import (`as e`) carries the
/// alias explicitly.
fn explicit_alias(m: &ImportRef) -> Option<String> {
    if m.alias == default_import_alias(&m.reference) {
        None
    } else {
        Some(m.alias.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_report::{DiffReport, EffectChanges, Modified, Renamed};

    fn dr() -> DiffReport { DiffReport::default() }

    #[test]
    fn empty_diff_yields_no_ops() {
        let head: BTreeMap<SigId, StageId> = BTreeMap::new();
        let eff: BTreeMap<SigId, EffectSet> = BTreeMap::new();
        let oi: ImportMap = ImportMap::new();
        let ni: ImportMap = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let d = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &stages,
            new_imports: &ni,
            diff: &d,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        assert!(ops.is_empty());
    }

    #[test]
    fn rename_emits_a_single_rename_op() {
        // Build a tiny new program with one fn under the new name.
        let src = "fn parse_int(s :: Str) -> Int { 0 }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let parse_int = stages.iter()
            .find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == "parse_int"))
            .cloned().unwrap();
        let to_sig = sig_id(&parse_int).unwrap();
        let to_stage = stage_id(&parse_int).unwrap();

        let mut head = BTreeMap::new();
        head.insert("parse-old-sig".to_string(), to_stage.clone());

        let mut diff = dr();
        diff.renamed.push(Renamed {
            from: "parse".into(),
            to: "parse_int".into(),
            signature: "fn parse_int(s :: Str) -> Int".into(),
            old_sig_id: "parse-old-sig".into(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &[parse_int],
            new_imports: &ni,
            diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::RenameSymbol { from, to, body_stage_id } => {
                assert_eq!(from, "parse-old-sig");
                assert_eq!(to, &to_sig);
                assert_eq!(body_stage_id, &to_stage);
            }
            other => panic!("expected RenameSymbol, got {other:?}"),
        }
    }

    #[test]
    fn body_only_modify_emits_modify_body() {
        let src = "fn fac(n :: Int) -> Int { 1 }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let fac = stages.iter().find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == "fac"))
            .cloned().unwrap();
        let sig = sig_id(&fac).unwrap();
        let new_stg = stage_id(&fac).unwrap();

        let mut head = BTreeMap::new();
        head.insert(sig.clone(), "old-stage-id".to_string());

        let mut diff = dr();
        diff.modified.push(Modified {
            name: "fac".into(),
            signature_before: "fn fac(n :: Int) -> Int".into(),
            signature_after:  "fn fac(n :: Int) -> Int".into(),
            signature_changed: false,
            effect_changes: EffectChanges::default(),
            body_patches: Vec::new(),
            old_sig_id: sig.clone(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_effects: &eff,
            old_imports: &oi, new_stages: &[fac], new_imports: &ni, diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::ModifyBody { sig_id: s, from_stage_id, to_stage_id, .. } => {
                assert_eq!(s, &sig);
                assert_eq!(from_stage_id, "old-stage-id");
                assert_eq!(to_stage_id, &new_stg);
            }
            other => panic!("expected ModifyBody, got {other:?}"),
        }
    }

    #[test]
    fn import_added_emits_add_import() {
        let mut new_imports = ImportMap::new();
        new_imports.insert("main.lex".into(),
            std::iter::once(ImportRef { reference: "std.io".into(), alias: "io".into() }).collect());
        let head = BTreeMap::new();
        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let diff = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &new_imports, diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::AddImport { in_file, module, alias } => {
                assert_eq!(in_file, "main.lex");
                assert_eq!(module, "std.io");
                // "io" is the default alias of "std.io", so it's omitted.
                assert_eq!(alias, &None);
            }
            other => panic!("expected AddImport, got {other:?}"),
        }
    }

    #[test]
    fn import_with_nondefault_alias_carries_it() {
        let mut new_imports = ImportMap::new();
        new_imports.insert("main.lex".into(),
            std::iter::once(ImportRef { reference: "./error".into(), alias: "e".into() }).collect());
        let head = BTreeMap::new();
        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let diff = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &new_imports, diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        match &ops[0] {
            OperationKind::AddImport { module, alias, .. } => {
                assert_eq!(module, "./error");
                // "e" != default alias "error", so it's carried explicitly.
                assert_eq!(alias, &Some("e".to_string()));
            }
            other => panic!("expected AddImport, got {other:?}"),
        }
    }

    /// #909: re-aliasing an import is a remove + add of the SAME reference. A
    /// `RemoveImport` carries no alias, so the remove must come FIRST or a
    /// replay (add, then remove-by-reference) leaves the import gone.
    #[test]
    fn realiasing_an_import_removes_before_it_adds() {
        for reference in ["./error", "std.sql"] {
            let mut old_imports = ImportMap::new();
            old_imports.insert("main.lex".into(),
                std::iter::once(ImportRef { reference: reference.into(), alias: "e".into() }).collect());
            let mut new_imports = ImportMap::new();
            new_imports.insert("main.lex".into(),
                std::iter::once(ImportRef { reference: reference.into(), alias: "err".into() }).collect());
            let head = BTreeMap::new();
            let eff = BTreeMap::new();
            let stages: Vec<Stage> = Vec::new();
            let diff = dr();
            let ops = diff_to_ops(DiffInputs {
                old_head: &head, old_effects: &eff,
                old_imports: &old_imports, new_stages: &stages, new_imports: &new_imports, diff: &diff,
                module_prefixes: &BTreeMap::new(),
            }).expect("ok");
            assert_eq!(ops.len(), 2, "{ops:?}");
            assert!(matches!(&ops[0], OperationKind::RemoveImport { module, .. } if module == reference), "{ops:?}");
            assert!(matches!(&ops[1], OperationKind::AddImport { alias: Some(a), .. } if a == "err"), "{ops:?}");
        }
    }

    #[test]
    fn a_default_alias_local_import_omits_the_alias() {
        let mut new_imports = ImportMap::new();
        new_imports.insert("src/main.lex".into(), [
            ImportRef { reference: "./error".into(), alias: "error".into() },
            ImportRef { reference: "./util/strings".into(), alias: "strings".into() },
        ].into_iter().collect());
        let head = BTreeMap::new();
        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let diff = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &new_imports, diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("ok");
        assert_eq!(ops.len(), 2);
        for op in &ops {
            assert!(matches!(op, OperationKind::AddImport { alias: None, .. }), "{op:?}");
        }
    }

    #[test]
    fn missing_old_sig_for_removed_name_errors() {
        let head: BTreeMap<SigId, StageId> = BTreeMap::new();
        let eff: BTreeMap<SigId, EffectSet> = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let mut diff = dr();
        // No `old_sig_id` was resolved for this removal — the caller
        // (e.g. a hand-assembled report, or a bug in the resolver)
        // failed to find the old side.
        diff.removed.push(crate::diff_report::AddRemove {
            name: "ghost".into(),
            signature: "fn ghost() -> Int".into(),
            old_sig_id: None,
        });
        let err = diff_to_ops(DiffInputs {
            old_head: &head, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &ni, diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).unwrap_err();
        match err {
            DiffMappingError::MissingOldSigForName(n) => assert_eq!(n, "ghost"),
            other => panic!("expected MissingOldSigForName, got {other:?}"),
        }
    }

    // ----------------------------- #223 acceptance ---------------------

    /// Bare effects must produce identical strings to pre-#223
    /// behavior — preserves OpId stability for every existing op log.
    /// Pre-#223 `effect_set` was `effs.iter().map(|e| e.name.clone())`,
    /// so the canonical form for `[net]` was `"net"`. Confirm that.
    #[test]
    fn bare_effect_set_string_is_unchanged_from_pre_223() {
        let src = "fn f() -> [net] Int { 0 }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let fd = match &stages[0] {
            Stage::FnDecl(fd) => fd,
            other => panic!("{other:?}"),
        };
        let set = effect_set(&fd.effects);
        assert_eq!(set, ["net".to_string()].into_iter().collect::<EffectSet>(),
            "bare [net] must canonicalize to {{\"net\"}} so existing \
             op logs keep their OpIds across the #223 change");
    }

    /// Parameterized effects produce a distinct, parens-quoted string
    /// — `[net("wttr.in")]` becomes `"net(\"wttr.in\")"`. This is the
    /// fulcrum that makes `[net]` → `[net("wttr.in")]` a real
    /// `ChangeEffectSig` op rather than a no-op.
    #[test]
    fn parameterized_effect_label_is_distinct_from_bare() {
        let bare_src = "fn f() -> [net] Int { 0 }";
        let scoped_src = r#"fn f() -> [net("wttr.in")] Int { 0 }"#;
        for (src, expected) in [
            (bare_src, vec!["net"]),
            (scoped_src, vec!["net(\"wttr.in\")"]),
        ] {
            let prog = lex_syntax::load_program_from_str(src).unwrap();
            let stages = lex_ast::canonicalize_program(&prog);
            let fd = match &stages[0] {
                Stage::FnDecl(fd) => fd,
                other => panic!("{other:?}"),
            };
            let want: EffectSet = expected.into_iter().map(String::from).collect();
            assert_eq!(effect_set(&fd.effects), want);
        }
    }

    /// End-to-end: when a function's effect declaration changes from
    /// `[net]` to `[net("wttr.in")]`, `diff_to_ops` must emit a
    /// `ChangeEffectSig` op carrying the parameterized form in
    /// `to_effects`. Pre-#223 this was a no-op (both flattened to
    /// `{"net"}`), defeating #207's reason to exist.
    #[test]
    fn changing_bare_to_parameterized_emits_change_effect_sig() {
        let bare_src   = "fn weather() -> [net] Str { \"\" }";
        let scoped_src = r#"fn weather() -> [net("wttr.in")] Str { "" }"#;

        let bare_stage = match &lex_ast::canonicalize_program(
            &lex_syntax::load_program_from_str(bare_src).unwrap())[0] {
            Stage::FnDecl(fd) => fd.clone(),
            _ => unreachable!(),
        };
        let scoped_stage = match &lex_ast::canonicalize_program(
            &lex_syntax::load_program_from_str(scoped_src).unwrap())[0] {
            Stage::FnDecl(fd) => fd.clone(),
            _ => unreachable!(),
        };

        let sig = sig_id(&Stage::FnDecl(bare_stage.clone())).unwrap();
        let from_stage_id = stage_id(&Stage::FnDecl(bare_stage.clone())).unwrap();

        let mut head = BTreeMap::new();
        head.insert(sig.clone(), from_stage_id.clone());
        let mut eff = BTreeMap::new();
        eff.insert(sig.clone(), effect_set(&bare_stage.effects));

        let mut diff = dr();
        diff.modified.push(Modified {
            name: "weather".into(),
            signature_before: "fn weather() -> [net] Str".into(),
            signature_after:  "fn weather() -> [net(\"wttr.in\")] Str".into(),
            signature_changed: true,
            body_patches: Vec::new(),
            effect_changes: EffectChanges {
                before: vec!["net".into()],
                after: vec!["net(\"wttr.in\")".into()],
                added: vec!["net(\"wttr.in\")".into()],
                removed: vec!["net".into()],
            },
            old_sig_id: sig.clone(),
        });

        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let new_stage = Stage::FnDecl(scoped_stage);
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &[new_stage],
            new_imports: &ni,
            diff: &diff,
            module_prefixes: &BTreeMap::new(),
        }).expect("diff_to_ops should succeed");

        let change = ops.iter().find(|op| matches!(op, OperationKind::ChangeEffectSig { .. }));
        let change = change.expect(
            "expected a ChangeEffectSig op when going [net] → [net(\"wttr.in\")] — \
             pre-#223 both sides flattened to {\"net\"} and the op was incorrectly \
             skipped");
        match change {
            OperationKind::ChangeEffectSig { from_effects, to_effects, .. } => {
                let from: Vec<_> = from_effects.iter().cloned().collect();
                let to:   Vec<_> = to_effects.iter().cloned().collect();
                assert_eq!(from, vec!["net".to_string()]);
                assert_eq!(to,   vec!["net(\"wttr.in\")".to_string()]);
            }
            _ => unreachable!(),
        }
    }
}
