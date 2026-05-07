//! M3: type checker. Walks the canonical AST, infers types via unification,
//! and checks declared signatures and effects.

use crate::builtins::{module_for_import, module_scope};
use crate::env::{TypeDefKind, TypeEnv, ty_from_canon};
use crate::error::TypeError;
use crate::types::*;
use crate::unifier::{UnifyError, Unifier};
use indexmap::IndexMap;
use lex_ast as a;
use std::collections::HashMap;

/// Result of checking a whole program.
pub struct ProgramTypes {
    pub fn_signatures: IndexMap<String, Scheme>,
    pub type_env: TypeEnv,
    /// For #168: per-call required-fields map for `module.parse(s)`
    /// calls whose inferred result type is `Result[Record{...}, _]`.
    /// Keyed by `&CExpr as *const _ as usize` so callers can do an
    /// O(1) pointer-equality lookup during a separate AST rewrite
    /// pass. Empty unless any matching call sites were found.
    ///
    /// See [`check_and_rewrite_program`] for the function that
    /// populates this and applies the rewrite in one step.
    pub parse_required_fields: HashMap<usize, Vec<String>>,
}

pub fn check_program(stages: &[a::Stage]) -> Result<ProgramTypes, Vec<TypeError>> {
    let mut tcx = Checker::new();
    let mut errors = Vec::new();

    // Pass 1: gather imports → bring module values into scope.
    for stage in stages {
        if let a::Stage::Import(i) = stage {
            if let Some(mod_name) = module_for_import(&i.reference) {
                if let Some(ty) = module_scope(mod_name, &tcx.type_env) {
                    tcx.globals.insert(i.alias.clone(), Scheme {
                        // Module-level signatures use Var(0..n) and
                        // effect-vars on stdlib HOFs (list.map's `[E]`
                        // etc.); generalize both.
                        vars: collect_vars(&ty),
                        eff_vars: collect_eff_vars(&ty),
                        ty,
                    });
                    tcx.module_aliases.insert(i.alias.clone(), mod_name.to_string());
                }
            }
        }
    }

    // Pass 2: register user-declared types.
    for stage in stages {
        if let a::Stage::TypeDecl(td) = stage {
            if let Err(e) = tcx.type_env.add_user_type(&td.name, td.clone()) {
                errors.push(TypeError::RecursiveTypeWithoutConstructor {
                    at_node: "n_0".into(),
                    name: e,
                });
            }
        }
    }

    // Pass 3: register fn signatures (so mutual recursion works).
    for stage in stages {
        if let a::Stage::FnDecl(fd) = stage {
            let scheme = function_scheme(fd);
            tcx.globals.insert(fd.name.clone(), scheme);
        }
    }

    // Pass 4: check each fn body.
    let mut signatures = IndexMap::new();
    for stage in stages {
        if let a::Stage::FnDecl(fd) = stage {
            match tcx.check_fn(fd) {
                Ok(scheme) => { signatures.insert(fd.name.clone(), scheme); }
                Err(es) => errors.extend(es),
            }
        }
    }

    if errors.is_empty() {
        // #168: walk pending parse-call records and resolve each
        // call's return type now that all unification has settled.
        // A call shows up here only if the call site syntactically
        // looks like `<alias>.parse(s)` for an alias bound to one
        // of {json, toml, yaml} via the import pass.
        let mut parse_required_fields = HashMap::new();
        for (call_ptr, ret_ty) in &tcx.pending_parse_calls {
            if let Some(fields) = extract_record_fields_from_result(&tcx.u, &tcx.type_env, ret_ty) {
                parse_required_fields.insert(*call_ptr, fields);
            }
        }
        Ok(ProgramTypes {
            fn_signatures: signatures,
            type_env: tcx.type_env,
            parse_required_fields,
        })
    } else {
        Err(errors)
    }
}

/// Type-check `stages` and rewrite every `module.parse(s)` call
/// where the inferred T is a Record into the equivalent
/// `module.parse_strict(s, [field_names])` (#168). Existing
/// [`check_program`] keeps the old immutable signature for tests
/// and tools that don't want the AST rewritten.
pub fn check_and_rewrite_program(
    stages: &mut [a::Stage],
) -> Result<ProgramTypes, Vec<TypeError>> {
    // Borrow as immutable for the type-check pass — the side-table
    // it produces is keyed by `*const CExpr as usize`, and the Vec
    // backing storage doesn't move between this borrow and the
    // mutable one below.
    let pt = check_program(&*stages)?;
    if !pt.parse_required_fields.is_empty() {
        rewrite_parse_calls(stages, &pt.parse_required_fields);
    }
    Ok(pt)
}

/// Walk `stages` mutably and, for every `CExpr::Call` whose
/// pointer (cast to `usize`) is a key in `required`, rewrite it
/// from `module.parse(s)` into `module.parse_strict(s, [...])`.
///
/// Assumptions:
///
/// - The `usize` keys come from the same physical AST passed
///   here. This is true when called from
///   [`check_and_rewrite_program`].
/// - Every key corresponds to a call whose callee is
///   `FieldAccess(_, "parse")`. The type-checker only inserts
///   keys when this holds, so we panic if the assumption is
///   violated — that's a checker bug, not a user error.
fn rewrite_parse_calls(stages: &mut [a::Stage], required: &HashMap<usize, Vec<String>>) {
    for stage in stages.iter_mut() {
        if let a::Stage::FnDecl(fd) = stage {
            rewrite_in_expr(&mut fd.body, required);
        }
    }
}

fn rewrite_in_expr(expr: &mut a::CExpr, required: &HashMap<usize, Vec<String>>) {
    let ptr = expr as *const a::CExpr as usize;
    let do_rewrite = required.get(&ptr).cloned();
    // Recurse into children first; rewriting the call itself
    // doesn't touch the source-arg, so the order doesn't change
    // semantics — but processing children up front means a
    // hypothetical nested parse-of-parse still gets rewritten
    // correctly.
    match expr {
        a::CExpr::Call { callee, args } => {
            rewrite_in_expr(callee, required);
            for a in args.iter_mut() { rewrite_in_expr(a, required); }
        }
        a::CExpr::Let { value, body, .. } => {
            rewrite_in_expr(value, required);
            rewrite_in_expr(body, required);
        }
        a::CExpr::Match { scrutinee, arms } => {
            rewrite_in_expr(scrutinee, required);
            for arm in arms.iter_mut() { rewrite_in_expr(&mut arm.body, required); }
        }
        a::CExpr::Block { statements, result } => {
            for s in statements.iter_mut() { rewrite_in_expr(s, required); }
            rewrite_in_expr(result, required);
        }
        a::CExpr::Constructor { args, .. } => {
            for a in args.iter_mut() { rewrite_in_expr(a, required); }
        }
        a::CExpr::RecordLit { fields } => {
            for f in fields.iter_mut() { rewrite_in_expr(&mut f.value, required); }
        }
        a::CExpr::TupleLit { items } | a::CExpr::ListLit { items } => {
            for it in items.iter_mut() { rewrite_in_expr(it, required); }
        }
        a::CExpr::FieldAccess { value, .. } => rewrite_in_expr(value, required),
        a::CExpr::Lambda { body, .. } => rewrite_in_expr(body, required),
        a::CExpr::BinOp { lhs, rhs, .. } => {
            rewrite_in_expr(lhs, required);
            rewrite_in_expr(rhs, required);
        }
        a::CExpr::UnaryOp { expr, .. } => rewrite_in_expr(expr, required),
        a::CExpr::Return { value } => rewrite_in_expr(value, required),
        a::CExpr::Literal { .. } | a::CExpr::Var { .. } => {}
    }
    if let Some(fields) = do_rewrite {
        match expr {
            a::CExpr::Call { callee, args } => {
                if let a::CExpr::FieldAccess { field, .. } = callee.as_mut() {
                    debug_assert_eq!(field, "parse",
                        "rewrite_in_expr: only `.parse` calls should be in the table");
                    *field = "parse_strict".to_string();
                }
                args.push(a::CExpr::ListLit {
                    items: fields.into_iter()
                        .map(|f| a::CExpr::Literal {
                            value: a::CLit::Str { value: f },
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
/// for `Result[Record{...}, _]`. Returns the field names if the
/// shape matches; `None` otherwise.
fn extract_record_fields_from_result(
    u: &Unifier,
    env: &TypeEnv,
    ty: &Ty,
) -> Option<Vec<String>> {
    let resolved = u.resolve(ty);
    let Ty::Con(ref name, ref args) = resolved else { return None; };
    if name != "Result" || args.len() != 2 { return None; }
    let ok_ty = u.resolve(&args[0]);
    let unfolded = unfold_record_alias_static(env, ok_ty);
    if let Ty::Record(fields) = unfolded {
        Some(fields.keys().cloned().collect())
    } else {
        None
    }
}

/// Standalone version of `Checker::unfold_record_alias` —
/// resolves a `Ty::Con` whose definition is a record alias to
/// the underlying record. Module-level helper because we need it
/// after the `Checker` has been moved/destructured.
fn unfold_record_alias_static(env: &TypeEnv, ty: Ty) -> Ty {
    if let Ty::Con(ref n, _) = ty {
        if let Some(td) = env.types.get(n) {
            if let TypeDefKind::Alias(inner @ Ty::Record(_)) = &td.kind {
                return inner.clone();
            }
        }
    }
    ty
}

fn collect_vars(t: &Ty) -> Vec<TyVarId> {
    let mut out = Vec::new();
    fn walk(t: &Ty, out: &mut Vec<TyVarId>) {
        match t {
            Ty::Var(v) => { if !out.contains(v) { out.push(*v); } }
            Ty::Prim(_) | Ty::Unit | Ty::Never => {}
            Ty::List(inner) => walk(inner, out),
            Ty::Tuple(items) => for it in items { walk(it, out); },
            Ty::Record(fs) => for v in fs.values() { walk(v, out); },
            Ty::Con(_, args) => for a in args { walk(a, out); },
            Ty::Function { params, ret, .. } => {
                for p in params { walk(p, out); }
                walk(ret, out);
            }
        }
    }
    walk(t, &mut out);
    out
}

/// Walk a type and collect every effect-row variable id that appears
/// inside any function-type's effect set. Used to generalize stdlib
/// HOF schemes alongside ordinary type vars.
fn collect_eff_vars(t: &Ty) -> Vec<u32> {
    let mut out = Vec::new();
    fn walk(t: &Ty, out: &mut Vec<u32>) {
        match t {
            Ty::Var(_) | Ty::Prim(_) | Ty::Unit | Ty::Never => {}
            Ty::List(inner) => walk(inner, out),
            Ty::Tuple(items) => for it in items { walk(it, out); },
            Ty::Record(fs) => for v in fs.values() { walk(v, out); },
            Ty::Con(_, args) => for a in args { walk(a, out); },
            Ty::Function { params, effects, ret } => {
                if let Some(v) = effects.var {
                    if !out.contains(&v) { out.push(v); }
                }
                for p in params { walk(p, out); }
                walk(ret, out);
            }
        }
    }
    walk(t, &mut out);
    out
}

fn function_scheme(fd: &a::FnDecl) -> Scheme {
    // Collect type-param ids in order; map their names to fresh Var(idx).
    let params: Vec<Ty> = fd.params.iter().map(|p| ty_from_canon(&p.ty, &fd.type_params)).collect();
    let ret = ty_from_canon(&fd.return_type, &fd.type_params);
    // Plumb effect args (#207). A canonical-AST `EffectDecl` already
    // carries `Option<EffectArg>`; map it into the type-system kind so
    // subsumption can honor parameterized effects.
    let effects = EffectSet {
        concrete: {
            let mut s = std::collections::BTreeSet::new();
            for e in &fd.effects {
                let arg = e.arg.as_ref().map(|a| match a {
                    a::EffectArg::Str { value } => crate::types::EffectArg::Str(value.clone()),
                    a::EffectArg::Int { value } => crate::types::EffectArg::Int(*value),
                    a::EffectArg::Ident { value } => crate::types::EffectArg::Ident(value.clone()),
                });
                s.insert(crate::types::EffectKind { name: e.name.clone(), arg });
            }
            s
        },
        var: None,
    };
    let ty = Ty::Function { params, effects, ret: Box::new(ret) };
    let vars: Vec<TyVarId> = (0..fd.type_params.len() as u32).collect();
    // User-declared functions don't carry effect-row variables today
    // (the surface syntax has no `[E]` form for user types). Only
    // stdlib HOFs do, and those are loaded via module_scope.
    Scheme { vars, eff_vars: Vec::new(), ty }
}

struct Checker {
    u: Unifier,
    type_env: TypeEnv,
    globals: IndexMap<String, Scheme>,
    /// Imported alias → canonical module name (e.g. `cfg` → `toml`).
    /// Populated during the import pass; consulted by `check_call`
    /// to recognise `cfg.parse(...)` as a stdlib parse call.
    module_aliases: IndexMap<String, String>,
    /// For #168: every `<alias>.parse(s)` call where alias is in
    /// `module_aliases` and maps to {json, toml, yaml}, recorded
    /// here as `(call_pointer_as_usize, return_type_var)`. After
    /// the whole program type-checks, we walk this and resolve
    /// each return type through the unifier — at that point any
    /// `Result[Manifest, _]` constraints from match patterns or
    /// let-annotations have settled.
    pending_parse_calls: Vec<(usize, Ty)>,
}

impl Checker {
    fn new() -> Self {
        Self {
            u: Unifier::new(),
            type_env: TypeEnv::new_with_builtins(),
            globals: IndexMap::new(),
            module_aliases: IndexMap::new(),
            pending_parse_calls: Vec::new(),
        }
    }

    /// If `ty` is a `Ty::Con(name, _)` whose definition is a record
    /// alias (`type Foo = { ... }`), return the inner record type.
    /// Otherwise return `ty` unchanged.
    fn unfold_record_alias(&self, ty: Ty) -> Ty {
        if let Ty::Con(ref n, _) = ty {
            if let Some(td) = self.type_env.types.get(n) {
                if let TypeDefKind::Alias(inner @ Ty::Record(_)) = &td.kind {
                    return inner.clone();
                }
            }
        }
        ty
    }

    /// Whether `callee` is a `<alias>.parse` field access where
    /// `<alias>` was imported from one of the stdlib modules whose
    /// `parse` returns `Result[T, Str]` and whose `parse_strict`
    /// shape exists for #168 enforcement (json / toml / yaml).
    fn is_module_parse_call(&self, callee: &a::CExpr) -> bool {
        if let a::CExpr::FieldAccess { value, field } = callee {
            if field != "parse" { return false; }
            if let a::CExpr::Var { name } = value.as_ref() {
                if let Some(module) = self.module_aliases.get(name) {
                    return matches!(module.as_str(), "json" | "toml" | "yaml");
                }
            }
        }
        false
    }

    /// Unify two types, asymmetrically coercing an anonymous record
    /// against a nominal record alias at any level of nesting. So a
    /// `{ x: 1, y: 2 }` literal can be passed to a fn taking
    /// `Inner = { x :: Int, y :: Int }`, even when the literal is the
    /// inner field of an outer record literal.
    ///
    /// We deliberately keep nominal-vs-nominal mismatches strict: two
    /// distinct `Ty::Con` names won't unify just because their record
    /// shapes match. The coercion fires only when one side is a bare
    /// `Ty::Record` and the other is a `Ty::Con` whose alias is a
    /// record.
    fn unify_with_record_coercion(&mut self, a: &Ty, b: &Ty) -> Result<(), UnifyError> {
        let a = self.u.resolve(a);
        let b = self.u.resolve(b);
        self.unify_coerce_inner(a, b)
    }

    fn unify_coerce_inner(&mut self, a: Ty, b: Ty) -> Result<(), UnifyError> {
        // Asymmetric Record↔Con(record-alias) coercion at this level.
        let (a, b) = match (&a, &b) {
            (Ty::Record(_), Ty::Con(_, _)) => (a, self.unfold_record_alias(b.clone())),
            (Ty::Con(_, _), Ty::Record(_)) => (self.unfold_record_alias(a.clone()), b),
            _ => (a, b),
        };

        match (&a, &b) {
            (Ty::Record(fa), Ty::Record(fb)) => {
                if fa.len() != fb.len() {
                    return Err(UnifyError::Mismatch { a: a.clone(), b: b.clone() });
                }
                for (k, va) in fa.clone() {
                    match fb.get(&k) {
                        Some(vb) => self.unify_coerce_inner(va, vb.clone())?,
                        None => return Err(UnifyError::Mismatch { a: a.clone(), b: b.clone() }),
                    }
                }
                Ok(())
            }
            (Ty::List(ta), Ty::List(tb)) => {
                self.unify_coerce_inner((**ta).clone(), (**tb).clone())
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.clone().into_iter().zip(ys.clone()) {
                    self.unify_coerce_inner(x, y)?;
                }
                Ok(())
            }
            _ => self.u.unify(&a, &b),
        }
    }

    fn check_fn(&mut self, fd: &a::FnDecl) -> Result<Scheme, Vec<TypeError>> {
        // Instantiate fn's signature with fresh vars for its type params.
        let scheme = function_scheme(fd);
        let (param_tys, declared_effects, ret_ty) = match instantiate(&scheme, &mut self.u) {
            Ty::Function { params, effects, ret } => (params, effects, *ret),
            _ => unreachable!(),
        };

        let mut locals: IndexMap<String, Ty> = IndexMap::new();
        for (p, t) in fd.params.iter().zip(param_tys.iter()) {
            locals.insert(p.name.clone(), t.clone());
        }

        let mut inferred_effects = EffectSet::empty();
        let body_ty = self.check_expr(&fd.body, "n_0", &mut locals, &mut inferred_effects)
            .map_err(|e| vec![e])?;

        // The body may produce an anonymous record literal where the
        // signature expects a nominal record alias (and vice-versa,
        // and at any nested level). `unify_with_record_coercion`
        // handles that asymmetry while keeping nominal-vs-nominal
        // mismatches strict.
        if let Err(e) = self.unify_with_record_coercion(&body_ty, &ret_ty) {
            return Err(vec![mismatch_err("n_0", e, &self.u, vec![format!("in function `{}`", fd.name)])]);
        }

        if !inferred_effects.is_subset(&declared_effects) {
            // Pick the first undeclared effect for the error.
            for e in inferred_effects.concrete.iter() {
                if !declared_effects.concrete.iter().any(|d| d.subsumes(e)) {
                    return Err(vec![TypeError::EffectNotDeclared {
                        at_node: "n_0".into(),
                        effect: e.pretty(),
                    }]);
                }
            }
        }

        Ok(scheme)
    }

    fn check_expr(
        &mut self,
        e: &a::CExpr,
        node_id: &str,
        locals: &mut IndexMap<String, Ty>,
        effs: &mut EffectSet,
    ) -> Result<Ty, TypeError> {
        match e {
            a::CExpr::Literal { value } => Ok(lit_type(value)),
            a::CExpr::Var { name } => {
                if let Some(t) = locals.get(name) {
                    return Ok(t.clone());
                }
                if let Some(scheme) = self.globals.get(name).cloned() {
                    return Ok(instantiate(&scheme, &mut self.u));
                }
                Err(TypeError::UnknownIdentifier { at_node: node_id.into(), name: name.clone() })
            }
            a::CExpr::Constructor { name, args } => self.check_constructor(name, args, node_id, locals, effs),
            a::CExpr::Call { callee, args } => self.check_call(e, callee, args, node_id, locals, effs),
            a::CExpr::Let { name, ty, value, body } => {
                let v_ty = self.check_expr(value, node_id, locals, effs)?;
                if let Some(declared) = ty {
                    let d = ty_from_canon(declared, &[]);
                    if let Err(err) = self.unify_with_record_coercion(&v_ty, &d) {
                        return Err(mismatch_err(node_id, err, &self.u, vec![format!("in let `{}`", name)]));
                    }
                }
                let prev = locals.insert(name.clone(), v_ty);
                let body_ty = self.check_expr(body, node_id, locals, effs)?;
                match prev {
                    Some(p) => { locals.insert(name.clone(), p); }
                    None => { locals.shift_remove(name); }
                }
                Ok(body_ty)
            }
            a::CExpr::Match { scrutinee, arms } => {
                let scrut_ty = self.check_expr(scrutinee, node_id, locals, effs)?;
                if arms.is_empty() {
                    return Err(TypeError::NonExhaustiveMatch {
                        at_node: node_id.into(), missing: vec!["_".into()]
                    });
                }
                let result_ty = self.u.fresh();
                for arm in arms {
                    let mut arm_locals = locals.clone();
                    self.bind_pattern(&arm.pattern, &scrut_ty, &mut arm_locals, node_id)?;
                    let arm_ty = self.check_expr(&arm.body, node_id, &mut arm_locals, effs)?;
                    if let Err(err) = self.unify_with_record_coercion(&arm_ty, &result_ty) {
                        return Err(mismatch_err(node_id, err, &self.u, vec!["in match arm".into()]));
                    }
                }
                Ok(result_ty)
            }
            a::CExpr::Block { statements, result } => {
                for s in statements {
                    self.check_expr(s, node_id, locals, effs)?;
                }
                self.check_expr(result, node_id, locals, effs)
            }
            a::CExpr::RecordLit { fields } => {
                let mut tys = IndexMap::new();
                for f in fields {
                    if tys.contains_key(&f.name) {
                        return Err(TypeError::DuplicateField {
                            at_node: node_id.into(), field: f.name.clone()
                        });
                    }
                    let ft = self.check_expr(&f.value, node_id, locals, effs)?;
                    tys.insert(f.name.clone(), ft);
                }
                Ok(Ty::Record(tys))
            }
            a::CExpr::TupleLit { items } => {
                let mut ts = Vec::new();
                for it in items { ts.push(self.check_expr(it, node_id, locals, effs)?); }
                Ok(Ty::Tuple(ts))
            }
            a::CExpr::ListLit { items } => {
                let elem = self.u.fresh();
                for it in items {
                    let t = self.check_expr(it, node_id, locals, effs)?;
                    if let Err(err) = self.unify_with_record_coercion(&t, &elem) {
                        return Err(mismatch_err(node_id, err, &self.u, vec!["in list literal".into()]));
                    }
                }
                Ok(Ty::List(Box::new(elem)))
            }
            a::CExpr::FieldAccess { value, field } => {
                let vt = self.check_expr(value, node_id, locals, effs)?;
                let resolved = self.u.resolve(&vt);
                // Unfold a Record-aliased Con (e.g. `type Request = { ... }`).
                let resolved = match resolved {
                    Ty::Con(ref n, _) => match self.type_env.types.get(n) {
                        Some(td) => match &td.kind {
                            TypeDefKind::Alias(inner @ Ty::Record(_)) => inner.clone(),
                            _ => resolved,
                        },
                        None => resolved,
                    },
                    other => other,
                };
                match resolved {
                    Ty::Record(fields) => fields.get(field).cloned()
                        .ok_or_else(|| TypeError::UnknownField {
                            at_node: node_id.into(),
                            record_type: Ty::Record(fields.clone()).pretty(),
                            field: field.clone(),
                        }),
                    other => Err(TypeError::TypeMismatch {
                        at_node: node_id.into(),
                        expected: "record".into(),
                        got: other.pretty(),
                        context: vec![format!("field access `.{}`", field)],
                    }),
                }
            }
            a::CExpr::Lambda { params, return_type, effects: l_effects, body } => {
                let param_tys: Vec<Ty> = params.iter().map(|p| ty_from_canon(&p.ty, &[])).collect();
                let ret_ty = ty_from_canon(return_type, &[]);
                let declared = EffectSet {
                    concrete: {
                        let mut s = std::collections::BTreeSet::new();
                        for e in l_effects {
                            let arg = e.arg.as_ref().map(|a| match a {
                                a::EffectArg::Str { value } => crate::types::EffectArg::Str(value.clone()),
                                a::EffectArg::Int { value } => crate::types::EffectArg::Int(*value),
                                a::EffectArg::Ident { value } => crate::types::EffectArg::Ident(value.clone()),
                            });
                            s.insert(crate::types::EffectKind { name: e.name.clone(), arg });
                        }
                        s
                    },
                    var: None,
                };
                let mut inner_locals = locals.clone();
                for (p, t) in params.iter().zip(param_tys.iter()) {
                    inner_locals.insert(p.name.clone(), t.clone());
                }
                let mut inner_effs = EffectSet::empty();
                let body_ty = self.check_expr(body, node_id, &mut inner_locals, &mut inner_effs)?;
                if let Err(err) = self.unify_with_record_coercion(&body_ty, &ret_ty) {
                    return Err(mismatch_err(node_id, err, &self.u, vec!["in lambda body".into()]));
                }
                if !inner_effs.is_subset(&declared) {
                    for e in inner_effs.concrete.iter() {
                        if !declared.concrete.iter().any(|d| d.subsumes(e)) {
                            return Err(TypeError::EffectNotDeclared {
                                at_node: node_id.into(),
                                effect: e.pretty(),
                            });
                        }
                    }
                }
                Ok(Ty::function(param_tys, declared, ret_ty))
            }
            a::CExpr::BinOp { op, lhs, rhs } => self.check_binop(op, lhs, rhs, node_id, locals, effs),
            a::CExpr::UnaryOp { op, expr } => {
                let t = self.check_expr(expr, node_id, locals, effs)?;
                match op.as_str() {
                    "-" => {
                        // Either Int or Float; we pick Int by default if unconstrained.
                        let r = self.u.resolve(&t);
                        match r {
                            Ty::Prim(Prim::Int) | Ty::Prim(Prim::Float) => Ok(t),
                            Ty::Var(_) => {
                                // default to Int.
                                self.u.unify(&t, &Ty::int()).map_err(|e| mismatch_err(node_id, e, &self.u, vec![]))?;
                                Ok(Ty::int())
                            }
                            other => Err(TypeError::TypeMismatch {
                                at_node: node_id.into(),
                                expected: "Int or Float".into(),
                                got: other.pretty(),
                                context: vec!["unary `-`".into()],
                            }),
                        }
                    }
                    "not" => {
                        self.u.unify(&t, &Ty::bool()).map_err(|e| mismatch_err(node_id, e, &self.u, vec!["unary `not`".into()]))?;
                        Ok(Ty::bool())
                    }
                    other => panic!("unknown unary op: {other}"),
                }
            }
            a::CExpr::Return { value } => {
                // For now treat Return as having type Never; the surrounding
                // context will unify with the actual return type.
                self.check_expr(value, node_id, locals, effs)?;
                Ok(Ty::Never)
            }
        }
    }

    fn check_binop(
        &mut self,
        op: &str,
        lhs: &a::CExpr,
        rhs: &a::CExpr,
        node_id: &str,
        locals: &mut IndexMap<String, Ty>,
        effs: &mut EffectSet,
    ) -> Result<Ty, TypeError> {
        let lt = self.check_expr(lhs, node_id, locals, effs)?;
        let rt = self.check_expr(rhs, node_id, locals, effs)?;
        match op {
            "+" | "-" | "*" | "/" | "%" => {
                self.u.unify(&lt, &rt).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                let r = self.u.resolve(&lt);
                match r {
                    Ty::Prim(Prim::Int) | Ty::Prim(Prim::Float) => Ok(lt),
                    Ty::Var(_) => {
                        self.u.unify(&lt, &Ty::int()).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                        Ok(Ty::int())
                    }
                    other => Err(TypeError::TypeMismatch {
                        at_node: node_id.into(),
                        expected: "Int or Float".into(),
                        got: other.pretty(),
                        context: vec![format!("operator `{op}`")],
                    }),
                }
            }
            "==" | "!=" => {
                self.u.unify(&lt, &rt).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                Ok(Ty::bool())
            }
            "<" | "<=" | ">" | ">=" => {
                self.u.unify(&lt, &rt).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                let r = self.u.resolve(&lt);
                match r {
                    Ty::Prim(Prim::Int) | Ty::Prim(Prim::Float) | Ty::Prim(Prim::Str) => Ok(Ty::bool()),
                    Ty::Var(_) => {
                        self.u.unify(&lt, &Ty::int()).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                        Ok(Ty::bool())
                    }
                    other => Err(TypeError::TypeMismatch {
                        at_node: node_id.into(),
                        expected: "Int, Float, or Str".into(),
                        got: other.pretty(),
                        context: vec![format!("operator `{op}`")],
                    }),
                }
            }
            "and" | "or" => {
                self.u.unify(&lt, &Ty::bool()).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                self.u.unify(&rt, &Ty::bool()).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("operator `{op}`")]))?;
                Ok(Ty::bool())
            }
            other => panic!("unknown binop: {other}"),
        }
    }

    fn check_call(
        &mut self,
        call_expr: &a::CExpr,
        callee: &a::CExpr,
        args: &[a::CExpr],
        node_id: &str,
        locals: &mut IndexMap<String, Ty>,
        effs: &mut EffectSet,
    ) -> Result<Ty, TypeError> {
        // #168: snapshot the call's address before the recursive
        // descent so we can later rewrite this exact node. Pointer
        // identity is only meaningful while the AST stays put,
        // which it does until check_program returns and the AST
        // is handed back to the caller. `is_module_parse_call`
        // recognises `<alias>.parse` where alias was bound to one
        // of {json, toml, yaml} during the import pass.
        let parse_call_ptr = if self.is_module_parse_call(callee) {
            Some(call_expr as *const a::CExpr as usize)
        } else {
            None
        };
        let callee_ty = self.check_expr(callee, node_id, locals, effs)?;
        let resolved = self.u.resolve(&callee_ty);
        match resolved {
            Ty::Function { params, effects, ret } => {
                if params.len() != args.len() {
                    return Err(TypeError::ArityMismatch {
                        at_node: node_id.into(),
                        expected: params.len(),
                        got: args.len(),
                    });
                }
                for (i, (a, p)) in args.iter().zip(params.iter()).enumerate() {
                    let at = self.check_expr(a, node_id, locals, effs)?;
                    if let Err(err) = self.unify_with_record_coercion(&at, p) {
                        return Err(mismatch_err(node_id, err, &self.u, vec![format!("argument {} of call", i + 1)]));
                    }
                }
                // Re-resolve effects after unifying args: an effect-row
                // variable on the function type may have been bound by
                // an argument's closure type, and we want the
                // *post-binding* set when propagating to the caller.
                let resolved_effects = self.u.resolve_effects(&effects);
                effs.extend(&resolved_effects);
                // #168: snapshot the post-arg-unification return type
                // for stdlib parse calls. Resolution to the eventual
                // `Result[Record{...}, _]` shape happens at the end
                // of `check_program` once the whole program's
                // unification has settled — match-pattern annotations
                // and let-type-annotations may bind T after this
                // point.
                if let Some(ptr) = parse_call_ptr {
                    self.pending_parse_calls.push((ptr, (*ret).clone()));
                }
                Ok(*ret)
            }
            Ty::Var(_) => {
                // Build a function type and unify.
                let mut p_tys = Vec::new();
                for a in args { p_tys.push(self.check_expr(a, node_id, locals, effs)?); }
                let r = self.u.fresh();
                let f = Ty::function(p_tys, EffectSet::empty(), r.clone());
                self.u.unify(&callee_ty, &f).map_err(|e| mismatch_err(node_id, e, &self.u, vec!["in call".into()]))?;
                Ok(r)
            }
            other => Err(TypeError::TypeMismatch {
                at_node: node_id.into(),
                expected: "function".into(),
                got: other.pretty(),
                context: vec!["in call".into()],
            }),
        }
    }

    fn check_constructor(
        &mut self,
        name: &str,
        args: &[a::CExpr],
        node_id: &str,
        locals: &mut IndexMap<String, Ty>,
        effs: &mut EffectSet,
    ) -> Result<Ty, TypeError> {
        let owning = self.type_env.ctor_to_type.get(name).cloned()
            .ok_or_else(|| TypeError::UnknownVariant {
                at_node: node_id.into(),
                constructor: name.to_string(),
            })?;
        let def = self.type_env.types.get(&owning).cloned()
            .expect("ctor_to_type points to a real type");
        let variants = match &def.kind {
            TypeDefKind::Union(v) => v.clone(),
            _ => return Err(TypeError::UnknownVariant {
                at_node: node_id.into(),
                constructor: name.to_string(),
            }),
        };
        // Instantiate the type's params with fresh vars; substitute into
        // both the variant's payload type and the resulting Con(...).
        let mut subst = IndexMap::new();
        let mut con_args = Vec::with_capacity(def.params.len());
        for (i, _p) in def.params.iter().enumerate() {
            let fresh = self.u.fresh();
            subst.insert(i as u32, fresh.clone());
            con_args.push(fresh);
        }
        let payload = variants.get(name).cloned().flatten();
        match (payload, args) {
            (None, []) => Ok(Ty::Con(owning, con_args)),
            (Some(payload), args) => {
                let inst_payload = subst_vars(&payload, &subst, &IndexMap::new());
                let arg_count = match &inst_payload {
                    Ty::Tuple(items) => items.len(),
                    _ => 1,
                };
                if arg_count != args.len() {
                    return Err(TypeError::ArityMismatch {
                        at_node: node_id.into(),
                        expected: arg_count,
                        got: args.len(),
                    });
                }
                if args.len() == 1 {
                    let at = self.check_expr(&args[0], node_id, locals, effs)?;
                    self.unify_with_record_coercion(&at, &inst_payload).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("constructor `{}`", name)]))?;
                } else if let Ty::Tuple(items) = inst_payload {
                    for (i, (a, t)) in args.iter().zip(items.iter()).enumerate() {
                        let at = self.check_expr(a, node_id, locals, effs)?;
                        self.unify_with_record_coercion(&at, t).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("constructor `{}` arg {}", name, i + 1)]))?;
                    }
                }
                Ok(Ty::Con(owning, con_args))
            }
            (None, _) => Err(TypeError::ArityMismatch {
                at_node: node_id.into(), expected: 0, got: args.len(),
            }),
        }
    }

    fn bind_pattern(
        &mut self,
        pat: &a::Pattern,
        ty: &Ty,
        locals: &mut IndexMap<String, Ty>,
        node_id: &str,
    ) -> Result<(), TypeError> {
        match pat {
            a::Pattern::PWild => Ok(()),
            a::Pattern::PVar { name } => {
                locals.insert(name.clone(), ty.clone());
                Ok(())
            }
            a::Pattern::PLiteral { value } => {
                let lt = lit_type(value);
                self.unify_with_record_coercion(&lt, ty).map_err(|e| mismatch_err(node_id, e, &self.u, vec!["in pattern".into()]))?;
                Ok(())
            }
            a::Pattern::PConstructor { name, args } => {
                // Re-use constructor logic but in pattern position.
                let owning = self.type_env.ctor_to_type.get(name).cloned()
                    .ok_or_else(|| TypeError::UnknownVariant {
                        at_node: node_id.into(), constructor: name.clone(),
                    })?;
                let def = self.type_env.types.get(&owning).cloned().unwrap();
                let mut subst = IndexMap::new();
                let mut con_args = Vec::new();
                for (i, _) in def.params.iter().enumerate() {
                    let fresh = self.u.fresh();
                    subst.insert(i as u32, fresh.clone());
                    con_args.push(fresh);
                }
                let con_ty = Ty::Con(owning.clone(), con_args);
                self.unify_with_record_coercion(&con_ty, ty).map_err(|e| mismatch_err(node_id, e, &self.u, vec![format!("constructor pattern `{}`", name)]))?;
                let payload = match &def.kind {
                    TypeDefKind::Union(v) => v.get(name).cloned().flatten(),
                    _ => None,
                };
                match (payload, args.as_slice()) {
                    (None, []) => Ok(()),
                    (Some(payload), args) => {
                        let inst = subst_vars(&payload, &subst, &IndexMap::new());
                        if args.len() == 1 {
                            self.bind_pattern(&args[0], &inst, locals, node_id)?;
                        } else if let Ty::Tuple(items) = inst {
                            for (a, t) in args.iter().zip(items.iter()) {
                                self.bind_pattern(a, t, locals, node_id)?;
                            }
                        }
                        Ok(())
                    }
                    (None, _) => Err(TypeError::ArityMismatch {
                        at_node: node_id.into(), expected: 0, got: args.len(),
                    }),
                }
            }
            a::Pattern::PRecord { fields } => {
                // Unfold a record-aliased Con (`type Bands = { ... }`)
                // so a structural `{ idea: pat, ... }` pattern can match
                // a nominal-typed scrutinee, mirror of #79's literal
                // coercion at every position.
                let resolved = self.unfold_record_alias(self.u.resolve(ty));
                let rec = match resolved {
                    Ty::Record(r) => r,
                    _ => return Err(TypeError::TypeMismatch {
                        at_node: node_id.into(),
                        expected: "record".into(),
                        got: ty.pretty(),
                        context: vec!["in record pattern".into()],
                    }),
                };
                for f in fields {
                    let ft = rec.get(&f.name).cloned()
                        .ok_or_else(|| TypeError::UnknownField {
                            at_node: node_id.into(),
                            record_type: Ty::Record(rec.clone()).pretty(),
                            field: f.name.clone(),
                        })?;
                    self.bind_pattern(&f.pattern, &ft, locals, node_id)?;
                }
                Ok(())
            }
            a::Pattern::PTuple { items } => {
                let resolved = self.u.resolve(ty);
                let tup = match resolved {
                    Ty::Tuple(t) => t,
                    Ty::Var(_) => {
                        let fresh: Vec<Ty> = items.iter().map(|_| self.u.fresh()).collect();
                        let tup_ty = Ty::Tuple(fresh.clone());
                        self.unify_with_record_coercion(&tup_ty, ty).map_err(|e| mismatch_err(node_id, e, &self.u, vec!["in tuple pattern".into()]))?;
                        fresh
                    }
                    other => return Err(TypeError::TypeMismatch {
                        at_node: node_id.into(),
                        expected: "tuple".into(),
                        got: other.pretty(),
                        context: vec!["in tuple pattern".into()],
                    }),
                };
                if tup.len() != items.len() {
                    return Err(TypeError::ArityMismatch {
                        at_node: node_id.into(), expected: tup.len(), got: items.len(),
                    });
                }
                for (p, t) in items.iter().zip(tup.iter()) {
                    self.bind_pattern(p, t, locals, node_id)?;
                }
                Ok(())
            }
        }
    }
}

fn lit_type(l: &a::CLit) -> Ty {
    match l {
        a::CLit::Int { .. } => Ty::int(),
        a::CLit::Float { .. } => Ty::float(),
        a::CLit::Str { .. } => Ty::str(),
        a::CLit::Bytes { .. } => Ty::bytes(),
        a::CLit::Bool { .. } => Ty::bool(),
        a::CLit::Unit => Ty::Unit,
    }
}

fn instantiate(s: &Scheme, u: &mut Unifier) -> Ty {
    let mut ty_subst = IndexMap::new();
    for v in &s.vars { ty_subst.insert(*v, u.fresh()); }
    let mut eff_subst = IndexMap::new();
    for v in &s.eff_vars { eff_subst.insert(*v, u.fresh_eff_id()); }
    subst_vars(&s.ty, &ty_subst, &eff_subst)
}

fn subst_vars(
    t: &Ty,
    subst: &IndexMap<TyVarId, Ty>,
    eff_subst: &IndexMap<u32, u32>,
) -> Ty {
    match t {
        Ty::Var(v) => subst.get(v).cloned().unwrap_or_else(|| Ty::Var(*v)),
        Ty::Prim(_) | Ty::Unit | Ty::Never => t.clone(),
        Ty::List(inner) => Ty::List(Box::new(subst_vars(inner, subst, eff_subst))),
        Ty::Tuple(items) => Ty::Tuple(items.iter().map(|t| subst_vars(t, subst, eff_subst)).collect()),
        Ty::Record(fs) => {
            let mut out = IndexMap::new();
            for (k, v) in fs { out.insert(k.clone(), subst_vars(v, subst, eff_subst)); }
            Ty::Record(out)
        }
        Ty::Con(n, args) => Ty::Con(n.clone(),
            args.iter().map(|t| subst_vars(t, subst, eff_subst)).collect()),
        Ty::Function { params, effects, ret } => {
            // Refresh the effect-row variable if it's quantified in the
            // scheme; concrete kinds carry through unchanged.
            let new_effects = EffectSet {
                concrete: effects.concrete.clone(),
                var: effects.var.and_then(|v| eff_subst.get(&v).copied()).or(effects.var),
            };
            Ty::Function {
                params: params.iter().map(|t| subst_vars(t, subst, eff_subst)).collect(),
                effects: new_effects,
                ret: Box::new(subst_vars(ret, subst, eff_subst)),
            }
        }
    }
}

fn mismatch_err(node_id: &str, e: UnifyError, u: &Unifier, context: Vec<String>) -> TypeError {
    match e {
        UnifyError::Mismatch { a, b } => TypeError::TypeMismatch {
            at_node: node_id.into(),
            expected: u.resolve(&b).pretty(),
            got: u.resolve(&a).pretty(),
            context,
        },
        UnifyError::Infinite { .. } => TypeError::InfiniteType { at_node: node_id.into() },
        UnifyError::EffectMismatch { a, b } => {
            // Render effect mismatches as a type-mismatch in compact
            // form, e.g. `[net]` vs `[]`. Avoids inventing a new
            // TypeError variant + wire format right now.
            let render = |e: &EffectSet| -> String {
                let mut parts: Vec<String> = e.concrete.iter()
                    .map(crate::types::EffectKind::pretty).collect();
                if let Some(v) = e.var { parts.push(format!("?e{}", v)); }
                if parts.is_empty() { "[]".into() } else { format!("[{}]", parts.join(", ")) }
            };
            TypeError::TypeMismatch {
                at_node: node_id.into(),
                expected: render(&b),
                got: render(&a),
                context,
            }
        }
    }
}
