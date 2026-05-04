//! Convert a `DiffReport` (+ import set deltas + old head info)
//! into a sequence of typed operations.

use crate::diff_report::DiffReport;
use crate::operation::{EffectSet, ModuleRef, OperationKind, SigId, StageId};
use lex_ast::{sig_id, stage_id, Effect, Stage};
use std::collections::{BTreeMap, BTreeSet};

pub type ImportMap = BTreeMap<String, BTreeSet<ModuleRef>>;

#[derive(Debug)]
pub struct DiffInputs<'a> {
    /// Current head SigId → StageId map.
    pub old_head: &'a BTreeMap<SigId, StageId>,
    /// Map of fn/type *name* → its SigId at the current head. The
    /// caller assembles this by walking the old stages or the metadata.
    pub old_name_to_sig: &'a BTreeMap<String, SigId>,
    /// Effect set per sig at the current head.
    pub old_effects: &'a BTreeMap<SigId, EffectSet>,
    /// Per-file imports at the current head.
    pub old_imports: &'a ImportMap,
    /// Stages of the new program (post-canonicalize).
    pub new_stages: &'a [Stage],
    /// Per-file imports of the new program.
    pub new_imports: &'a ImportMap,
    /// AST-diff between old and new sources, by name.
    pub diff: &'a DiffReport,
}

pub fn diff_to_ops(inputs: DiffInputs<'_>) -> Vec<OperationKind> {
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
        for m in modules.difference(&old) {
            out.push(OperationKind::AddImport {
                in_file: file.clone(),
                module: m.clone(),
            });
        }
        for m in old.difference(modules) {
            out.push(OperationKind::RemoveImport {
                in_file: file.clone(),
                module: m.clone(),
            });
        }
    }
    for (file, old) in inputs.old_imports {
        if !inputs.new_imports.contains_key(file) {
            for m in old {
                out.push(OperationKind::RemoveImport {
                    in_file: file.clone(),
                    module: m.clone(),
                });
            }
        }
    }

    // 2. Removed → RemoveFunction / RemoveType.
    for r in &inputs.diff.removed {
        let Some(sig) = inputs.old_name_to_sig.get(&r.name) else { continue; };
        let Some(last) = inputs.old_head.get(sig) else { continue; };
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
        let Some(stage) = new_by_name.get(a.name.as_str()) else { continue; };
        let Some(sig) = sig_id(stage) else { continue; };
        let Some(stg) = stage_id(stage) else { continue; };
        match stage {
            Stage::FnDecl(fd) => {
                let effects = effect_set(&fd.effects);
                out.push(OperationKind::AddFunction {
                    sig_id: sig, stage_id: stg, effects,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::AddType { sig_id: sig, stage_id: stg });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    // 4. Renamed → RenameSymbol.
    for r in &inputs.diff.renamed {
        let Some(from_sig) = inputs.old_name_to_sig.get(&r.from) else { continue; };
        let Some(stage) = new_by_name.get(r.to.as_str()) else { continue; };
        let Some(to_sig) = sig_id(stage) else { continue; };
        let Some(body_id) = stage_id(stage) else { continue; };
        out.push(OperationKind::RenameSymbol {
            from: from_sig.clone(),
            to: to_sig,
            body_stage_id: body_id,
        });
    }

    // 5. Modified → ChangeEffectSig | ModifyBody | ModifyType.
    for m in &inputs.diff.modified {
        let Some(sig) = inputs.old_name_to_sig.get(&m.name) else { continue; };
        let Some(from_id) = inputs.old_head.get(sig) else { continue; };
        let Some(stage) = new_by_name.get(m.name.as_str()) else { continue; };
        let Some(to_id) = stage_id(stage) else { continue; };
        let effects_changed =
            !m.effect_changes.added.is_empty() || !m.effect_changes.removed.is_empty();
        match stage {
            Stage::FnDecl(fd) if effects_changed => {
                let from_effects = inputs.old_effects.get(sig).cloned().unwrap_or_default();
                let to_effects = effect_set(&fd.effects);
                out.push(OperationKind::ChangeEffectSig {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                    from_effects,
                    to_effects,
                });
            }
            Stage::FnDecl(_) => {
                out.push(OperationKind::ModifyBody {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::ModifyType {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    out
}

fn effect_set(effs: &[Effect]) -> EffectSet {
    effs.iter().map(|e| e.name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_report::{DiffReport, EffectChanges, Modified, Renamed};

    fn dr() -> DiffReport { DiffReport::default() }

    #[test]
    fn empty_diff_yields_no_ops() {
        let head: BTreeMap<SigId, StageId> = BTreeMap::new();
        let n2s: BTreeMap<String, SigId> = BTreeMap::new();
        let eff: BTreeMap<SigId, EffectSet> = BTreeMap::new();
        let oi: ImportMap = ImportMap::new();
        let ni: ImportMap = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let d = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_name_to_sig: &n2s,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &stages,
            new_imports: &ni,
            diff: &d,
        });
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
        let mut n2s = BTreeMap::new();
        n2s.insert("parse".to_string(), "parse-old-sig".to_string());

        let mut diff = dr();
        diff.renamed.push(Renamed {
            from: "parse".into(),
            to: "parse_int".into(),
            signature: "fn parse_int(s :: Str) -> Int".into(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_name_to_sig: &n2s,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &[parse_int],
            new_imports: &ni,
            diff: &diff,
        });
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
        let mut n2s = BTreeMap::new();
        n2s.insert("fac".to_string(), sig.clone());

        let mut diff = dr();
        diff.modified.push(Modified {
            name: "fac".into(),
            signature_before: "fn fac(n :: Int) -> Int".into(),
            signature_after:  "fn fac(n :: Int) -> Int".into(),
            signature_changed: false,
            effect_changes: EffectChanges::default(),
            body_patches: Vec::new(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_name_to_sig: &n2s, old_effects: &eff,
            old_imports: &oi, new_stages: &[fac], new_imports: &ni, diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::ModifyBody { sig_id: s, from_stage_id, to_stage_id } => {
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
            std::iter::once("std.io".to_string()).collect());
        let head = BTreeMap::new();
        let n2s = BTreeMap::new();
        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let diff = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_name_to_sig: &n2s, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &new_imports, diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::AddImport { in_file, module } => {
                assert_eq!(in_file, "main.lex");
                assert_eq!(module, "std.io");
            }
            other => panic!("expected AddImport, got {other:?}"),
        }
    }
}
