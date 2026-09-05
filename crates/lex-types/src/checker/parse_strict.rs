//! The `parse` → `parse_strict_typed` / `json_body` → `json_body_typed`
//! rewrite (#168, #322, #684): call-site identity ([`ParseSite`], #777),
//! the record-schema extraction that decides whether a decode call is
//! rewritten, and the AST rewrite pass itself. The checker records
//! candidate sites while checking calls (see `super`); the rewrite
//! runs after the whole program has unified.

use super::*;

/// Field names + type-tag schema extracted from a `Result[Record{...}, _]`
/// return type. Used by the `parse` → `parse_strict_typed` rewrite (#322).
pub(super) type FieldSchema = (Vec<String>, Vec<(String, String)>);

/// Stable identity of a call site inside a checked program (#777).
///
/// `stage` is the index of the enclosing stage in the slice handed to
/// [`check_program`]; `node` is the positional [`a::NodeId`] path of
/// the call expression within that stage (`n_0.<i>...`, see
/// `lex_ast::ids`). Both survive cloning, serialisation and re-walking
/// the AST, unlike the raw `*const CExpr` addresses the checker used
/// to key its side tables by — so a [`ProgramTypes`] can be applied
/// to a *copy* of the stages it was computed from, and the same key
/// can be reported in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParseSite {
    pub stage: usize,
    pub node: a::NodeId,
}

/// Apply the `parse` → `parse_strict_typed` / `json_body` →
/// `json_body_typed` rewrite recorded in `pt` to `stages`.
///
/// `stages` must be structurally identical to the slice `pt` was
/// computed from — the same stages in the same order — but need not
/// be the same allocation: the side tables are keyed by
/// [`ParseSite`] (stage index + NodeId), so a deep clone, or a copy
/// that round-tripped through serialisation, rewrites identically
/// (#777). [`check_and_rewrite_program`] is the one-step form.
///
/// Every recorded site names a call whose callee is a `FieldAccess`
/// on a decode op. The type-checker only records sites when this
/// holds, so a mismatch is a checker bug and panics rather than
/// silently skipping the rewrite.
pub fn rewrite_parse_calls(stages: &mut [a::Stage], pt: &ProgramTypes) {
    if pt.parse_required_fields.is_empty() {
        return;
    }
    for (stage_idx, stage) in stages.iter_mut().enumerate() {
        if !pt.parse_required_fields.keys().any(|s| s.stage == stage_idx) {
            continue;
        }
        // Resolve this stage's NodeIds to addresses *in this AST*,
        // then hand the address-keyed tables to the walk below. The
        // map holds raw pointers only (no borrow), so the mutable
        // walk that follows is fine.
        let ptr_of: HashMap<a::NodeId, usize> = a::expr_ids(&*stage)
            .into_iter()
            .map(|(p, id)| (id, p as usize))
            .collect();
        let resolve = |site: &ParseSite| -> Option<usize> {
            (site.stage == stage_idx).then(|| {
                *ptr_of.get(&site.node).unwrap_or_else(|| panic!(
                    "rewrite_parse_calls: {:?} names no expression in stage {stage_idx}; \
                     the stages differ from the ones that were type-checked",
                    site.node))
            })
        };
        let required: HashMap<usize, Vec<String>> = pt.parse_required_fields.iter()
            .filter_map(|(site, f)| resolve(site).map(|p| (p, f.clone())))
            .collect();
        let schemas: HashMap<usize, Vec<(String, String)>> = pt.parse_type_schemas.iter()
            .filter_map(|(site, s)| resolve(site).map(|p| (p, s.clone())))
            .collect();
        if let a::Stage::FnDecl(fd) = stage {
            rewrite_in_expr(&mut fd.body, &required, &schemas);
        }
    }
}

/// Address-keyed inner walk for [`rewrite_parse_calls`]. `required`
/// and `schemas` are keyed by `&CExpr as *const _ as usize` and must
/// have been resolved against *this* `expr` tree (the caller does
/// that per stage from the NodeId-keyed tables).
pub(super) fn rewrite_in_expr(
    expr: &mut a::CExpr,
    required: &HashMap<usize, Vec<String>>,
    schemas: &HashMap<usize, Vec<(String, String)>>,
) {
    let ptr = expr as *const a::CExpr as usize;
    let do_rewrite = required.get(&ptr).cloned();
    let do_schema = schemas.get(&ptr).cloned();
    // Recurse into children first; rewriting the call itself
    // doesn't touch the source-arg, so the order doesn't change
    // semantics — but processing children up front means a
    // hypothetical nested parse-of-parse still gets rewritten
    // correctly.
    match expr {
        a::CExpr::Call { callee, args } => {
            rewrite_in_expr(callee, required, schemas);
            for a in args.iter_mut() { rewrite_in_expr(a, required, schemas); }
        }
        a::CExpr::Let { value, body, .. } => {
            rewrite_in_expr(value, required, schemas);
            rewrite_in_expr(body, required, schemas);
        }
        a::CExpr::Match { scrutinee, arms } => {
            rewrite_in_expr(scrutinee, required, schemas);
            for arm in arms.iter_mut() { rewrite_in_expr(&mut arm.body, required, schemas); }
        }
        a::CExpr::Block { statements, result } => {
            for s in statements.iter_mut() { rewrite_in_expr(s, required, schemas); }
            rewrite_in_expr(result, required, schemas);
        }
        a::CExpr::Constructor { args, .. } => {
            for a in args.iter_mut() { rewrite_in_expr(a, required, schemas); }
        }
        a::CExpr::RecordLit { fields } => {
            for f in fields.iter_mut() { rewrite_in_expr(&mut f.value, required, schemas); }
        }
        a::CExpr::TupleLit { items } | a::CExpr::ListLit { items } => {
            for it in items.iter_mut() { rewrite_in_expr(it, required, schemas); }
        }
        a::CExpr::FieldAccess { value, .. } => rewrite_in_expr(value, required, schemas),
        a::CExpr::Lambda { body, .. } => rewrite_in_expr(body, required, schemas),
        a::CExpr::BinOp { lhs, rhs, .. } => {
            rewrite_in_expr(lhs, required, schemas);
            rewrite_in_expr(rhs, required, schemas);
        }
        a::CExpr::UnaryOp { expr, .. } => rewrite_in_expr(expr, required, schemas),
        a::CExpr::Return { value } => rewrite_in_expr(value, required, schemas),
        a::CExpr::Literal { .. } | a::CExpr::Var { .. } => {}
    }
    if let Some(fields) = do_rewrite {
        match expr {
            a::CExpr::Call { callee, args } => {
                if let a::CExpr::FieldAccess { field, .. } = callee.as_mut() {
                    // Map each public decode op to its internal typed variant
                    // (3-arg: source, required-fields, schema) so direct
                    // callers of the public op aren't broken.
                    let typed = match field.as_str() {
                        "parse" => "parse_strict_typed",     // json / toml / yaml
                        "json_body" => "json_body_typed",    // http (#684)
                        other => unreachable!(
                            "rewrite_in_expr: unexpected decode field `{other}`"),
                    };
                    *field = typed.to_string();
                }
                // Second argument: List[Str] of required field names.
                args.push(a::CExpr::ListLit {
                    items: fields.into_iter()
                        .map(|f| a::CExpr::Literal {
                            value: a::CLit::Str { value: f },
                        })
                        .collect(),
                });
                // Third argument: List[(Str, Str)] type schema (#322).
                let schema = do_schema.unwrap_or_default();
                args.push(a::CExpr::ListLit {
                    items: schema.into_iter()
                        .map(|(name, tag)| a::CExpr::TupleLit {
                            items: vec![
                                a::CExpr::Literal { value: a::CLit::Str { value: name } },
                                a::CExpr::Literal { value: a::CLit::Str { value: tag } },
                            ],
                        })
                        .collect(),
                });
            }
            _ => unreachable!("rewrite table key must point to a Call expression"),
        }
    }
}

/// Given an inferred return type from a `module.parse(s)` call,
/// resolve through the unifier and any type aliases, then look
/// for `Result[Record{...}, _]`. Returns `(field_names, schema)`
/// where `schema` is a `Vec<(field_name, type_tag)>` for #322.
/// Returns `None` if the shape doesn't match.
pub(super) fn extract_record_fields_and_schema(
    u: &Unifier,
    env: &TypeEnv,
    ty: &Ty,
) -> Option<FieldSchema> {
    let resolved = u.resolve(ty);
    let Ty::Con(ref name, ref args) = resolved else { return None; };
    if name != "Result" || args.len() != 2 { return None; }
    let ok_ty = u.resolve(&args[0]);
    let unfolded = unfold_record_alias_static(env, ok_ty);
    if let Ty::Record(fields) = unfolded {
        let schema: Vec<(String, String)> = fields.iter()
            .map(|(k, v)| (k.clone(), ty_to_tag(u, v)))
            .collect();
        // Only non-Option fields are *required*: an `Option[T]` field is
        // satisfied by absence (it decodes to `None`), so it must not be
        // in the required list or an absent optional would wrongly fail
        // `check_required_fields`. The schema still carries every field
        // (for type validation + `apply_option_wrapping`).
        let names: Vec<String> = schema.iter()
            .filter(|(_, tag)| !tag.starts_with("Option["))
            .map(|(k, _)| k.clone())
            .collect();
        Some((names, schema))
    } else {
        None
    }
}

/// Convert a `Ty` to a compact string tag for the type schema
/// injected by the rewrite pass (#322). The runtime uses these
/// tags to validate JSON field values against the declared Lex type.
pub(super) fn ty_to_tag(u: &Unifier, ty: &Ty) -> String {
    let resolved = u.resolve(ty);
    match &resolved {
        Ty::Prim(Prim::Int)   => "Int".to_string(),
        Ty::Prim(Prim::Float) => "Float".to_string(),
        Ty::Prim(Prim::Bool)  => "Bool".to_string(),
        Ty::Prim(Prim::Str)   => "Str".to_string(),
        Ty::Con(name, args) if name == "Option" && args.len() == 1 => {
            format!("Option[{}]", ty_to_tag(u, &args[0]))
        }
        Ty::List(inner) => {
            format!("List[{}]", ty_to_tag(u, inner))
        }
        Ty::Record(_) => "Record".to_string(),
        _ => "Any".to_string(),
    }
}

/// Standalone version of `Checker::unfold_record_alias` —
/// resolves a `Ty::Con` whose definition is a type alias (record
/// or otherwise) to the underlying type. Module-level helper
/// because we need it after the `Checker` has been
/// moved/destructured.
pub(super) fn unfold_record_alias_static(env: &TypeEnv, ty: Ty) -> Ty {
    if let Ty::Con(ref n, ref args) = ty {
        if let Some(td) = env.types.get(n) {
            if let TypeDefKind::Alias(inner) = &td.kind {
                if td.params.len() != args.len() {
                    return ty;
                }
                if td.params.is_empty() {
                    return inner.clone();
                }
                let mut subst = IndexMap::new();
                for (i, a) in args.iter().enumerate() {
                    subst.insert(i as u32, a.clone());
                }
                return subst_vars(inner, &subst, &IndexMap::new());
            }
        }
    }
    ty
}

impl Checker {
    /// Whether `callee` is a stdlib decode call eligible for the #168 /
    /// #322 required-field + type-schema rewrite. Two shapes qualify:
    /// `<alias>.parse` for an alias bound to json / toml / yaml (returns
    /// `Result[T, Str]`), and `<alias>.json_body` for an alias bound to
    /// http (#684) — the most common API-decode path, which returns
    /// `Result[T, HttpError]` and was previously unvalidated. In both
    /// cases the rewrite only fires when the inferred `T` is a record
    /// (see `extract_record_fields_and_schema`).
    /// True when some import alias resolves to a module with a
    /// rewritable decode op (see `is_module_parse_call`). Gates the
    /// per-FnDecl NodeId walk in `check_program_inner` (#777).
    pub(super) fn has_parse_capable_imports(&self) -> bool {
        self.module_aliases.values()
            .any(|m| matches!(m.as_str(), "json" | "toml" | "yaml" | "http"))
    }

    /// Stable [`ParseSite`] for `call_expr`, which must belong to the
    /// FnDecl currently being checked. `None` when the call has no
    /// NodeId — the only such expressions are inside `examples {}`
    /// blocks, which the rewrite pass never touched anyway.
    pub(super) fn parse_site_of(&self, call_expr: &a::CExpr) -> Option<ParseSite> {
        let (stage, ids) = self.stage_ids.as_ref()?;
        let node = ids.get(&(call_expr as *const a::CExpr))?.clone();
        Some(ParseSite { stage: *stage, node })
    }

    pub(super) fn is_module_parse_call(&self, callee: &a::CExpr) -> bool {
        if let a::CExpr::FieldAccess { value, field } = callee {
            if let a::CExpr::Var { name } = value.as_ref() {
                if let Some(module) = self.module_aliases.get(name) {
                    return matches!(
                        (module.as_str(), field.as_str()),
                        ("json" | "toml" | "yaml", "parse") | ("http", "json_body")
                    );
                }
            }
        }
        false
    }
}
