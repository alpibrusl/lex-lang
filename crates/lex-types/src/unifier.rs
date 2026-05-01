//! Union-find based unification for type variables.

use crate::types::*;
use indexmap::IndexMap;

#[derive(Default)]
pub struct Unifier {
    next_var: TyVarId,
    /// Substitutions: `subst[v] = t` means var `v` was bound to `t`.
    subst: IndexMap<TyVarId, Ty>,
    /// Effect-row substitutions: `eff_subst[v] = set` means effect-var
    /// `v` was bound to `set` (which may itself carry another var,
    /// so resolve_effects walks the chain).
    eff_subst: IndexMap<u32, EffectSet>,
    /// Counter for fresh effect-row variables, separate from type
    /// variables to keep the namespaces clean.
    next_eff_var: u32,
}

impl Unifier {
    pub fn new() -> Self { Self::default() }

    pub fn fresh(&mut self) -> Ty {
        let v = self.next_var;
        self.next_var += 1;
        Ty::Var(v)
    }

    pub fn fresh_id(&mut self) -> TyVarId {
        let v = self.next_var;
        self.next_var += 1;
        v
    }

    /// Allocate a fresh effect-row variable for use in polymorphic
    /// signatures (e.g. `list.map[T, U, E]`'s `E`).
    pub fn fresh_eff_id(&mut self) -> u32 {
        let v = self.next_eff_var;
        self.next_eff_var += 1;
        v
    }

    /// Resolve a type by following substitutions. Recursive; structural.
    pub fn resolve(&self, t: &Ty) -> Ty {
        match t {
            Ty::Var(v) => match self.subst.get(v) {
                Some(t2) => self.resolve(t2),
                None => Ty::Var(*v),
            },
            Ty::Prim(_) | Ty::Unit | Ty::Never => t.clone(),
            Ty::List(inner) => Ty::List(Box::new(self.resolve(inner))),
            Ty::Tuple(items) => Ty::Tuple(items.iter().map(|t| self.resolve(t)).collect()),
            Ty::Record(fs) => {
                let mut out = IndexMap::new();
                for (k, v) in fs { out.insert(k.clone(), self.resolve(v)); }
                Ty::Record(out)
            }
            Ty::Con(n, args) => Ty::Con(n.clone(), args.iter().map(|t| self.resolve(t)).collect()),
            Ty::Function { params, effects, ret } => Ty::Function {
                params: params.iter().map(|t| self.resolve(t)).collect(),
                effects: self.resolve_effects(effects),
                ret: Box::new(self.resolve(ret)),
            },
        }
    }

    /// Resolve an effect set by chasing the `var` substitution chain.
    /// Concrete effects accumulate along the chain; the returned set's
    /// `var` is the terminal unbound var, or `None` if fully concrete.
    pub fn resolve_effects(&self, eff: &EffectSet) -> EffectSet {
        let mut out = EffectSet { concrete: eff.concrete.clone(), var: None };
        let mut cur_var = eff.var;
        while let Some(v) = cur_var {
            match self.eff_subst.get(&v) {
                Some(bound) => {
                    out.concrete.extend(bound.concrete.iter().cloned());
                    cur_var = bound.var;
                }
                None => { out.var = Some(v); break; }
            }
        }
        out
    }

    /// Unify two effect sets. Variables are existentially bound at the
    /// signature site; at call sites they bind to the actual closure's
    /// effects.
    ///
    /// Cases (after resolving):
    ///   - both fully concrete: must be equal
    ///   - exactly one carries a var: var := the *missing* effects
    ///     (i.e. the other side's concrete minus this side's), with
    ///     the other side's residual var if any
    ///   - both carry a var: bind one to the other (alias)
    pub fn unify_effects(&mut self, a: &EffectSet, b: &EffectSet) -> Result<(), UnifyError> {
        let a = self.resolve_effects(a);
        let b = self.resolve_effects(b);
        match (a.var, b.var) {
            (None, None) => {
                if a.concrete == b.concrete { Ok(()) }
                else { Err(UnifyError::EffectMismatch { a, b }) }
            }
            (Some(va), Some(vb)) if va == vb => {
                if a.concrete == b.concrete { Ok(()) }
                else { Err(UnifyError::EffectMismatch { a, b }) }
            }
            (Some(va), _) => {
                // Bind va so a + bound = b. Means bound has b.concrete
                // minus a.concrete, plus b.var.
                if !a.concrete.is_subset(&b.concrete) {
                    // a says "at least these" but b doesn't have all
                    // of them and isn't open enough to absorb. Reject.
                    if b.var.is_none() {
                        return Err(UnifyError::EffectMismatch { a, b });
                    }
                    // b has a var; we can absorb a's extras into b's var
                    // by binding b's var symmetrically. Easier: just
                    // bind va to (b's concrete + b's var) and rely on
                    // the chain to track. Tighter handling possible
                    // later.
                }
                let extra: std::collections::BTreeSet<String> =
                    b.concrete.difference(&a.concrete).cloned().collect();
                let bound = EffectSet { concrete: extra, var: b.var };
                self.eff_subst.insert(va, bound);
                Ok(())
            }
            (None, Some(vb)) => {
                if !b.concrete.is_subset(&a.concrete) {
                    return Err(UnifyError::EffectMismatch { a, b });
                }
                let extra: std::collections::BTreeSet<String> =
                    a.concrete.difference(&b.concrete).cloned().collect();
                let bound = EffectSet { concrete: extra, var: None };
                self.eff_subst.insert(vb, bound);
                Ok(())
            }
        }
    }

    pub fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), UnifyError> {
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            (Ty::Var(v), other) | (other, Ty::Var(v)) => {
                if let Ty::Var(w) = other {
                    if v == w { return Ok(()); }
                }
                if occurs(*v, other, self) {
                    return Err(UnifyError::Infinite { var: *v, ty: other.clone() });
                }
                self.subst.insert(*v, other.clone());
                Ok(())
            }
            (Ty::Prim(p1), Ty::Prim(p2)) if p1 == p2 => Ok(()),
            (Ty::Unit, Ty::Unit) | (Ty::Never, Ty::Never) => Ok(()),
            // Never is a subtype of everything (bottom). For unification we
            // treat it as compatible with any type.
            (Ty::Never, _) | (_, Ty::Never) => Ok(()),
            (Ty::List(t1), Ty::List(t2)) => self.unify(t1, t2),
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys.iter()) { self.unify(x, y)?; }
                Ok(())
            }
            (Ty::Record(a), Ty::Record(b)) => {
                if a.len() != b.len() {
                    return Err(UnifyError::Mismatch { a: Ty::Record(a.clone()), b: Ty::Record(b.clone()) });
                }
                for (k, va) in a {
                    match b.get(k) {
                        Some(vb) => self.unify(va, vb)?,
                        None => return Err(UnifyError::Mismatch {
                            a: Ty::Record(a.clone()), b: Ty::Record(b.clone())
                        }),
                    }
                }
                Ok(())
            }
            (Ty::Con(n1, a1), Ty::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
                for (x, y) in a1.iter().zip(a2.iter()) { self.unify(x, y)?; }
                Ok(())
            }
            (Ty::Function { params: p1, effects: e1, ret: r1 },
             Ty::Function { params: p2, effects: e2, ret: r2 })
                if p1.len() == p2.len() =>
            {
                for (x, y) in p1.iter().zip(p2.iter()) { self.unify(x, y)?; }
                self.unify_effects(e1, e2)?;
                self.unify(r1, r2)
            }
            _ => Err(UnifyError::Mismatch { a, b }),
        }
    }
}

fn occurs(v: TyVarId, t: &Ty, u: &Unifier) -> bool {
    let t = u.resolve(t);
    match t {
        Ty::Var(w) => v == w,
        Ty::Prim(_) | Ty::Unit | Ty::Never => false,
        Ty::List(inner) => occurs(v, &inner, u),
        Ty::Tuple(items) => items.iter().any(|t| occurs(v, t, u)),
        Ty::Record(fs) => fs.values().any(|t| occurs(v, t, u)),
        Ty::Con(_, args) => args.iter().any(|t| occurs(v, t, u)),
        Ty::Function { params, ret, .. } => {
            params.iter().any(|t| occurs(v, t, u)) || occurs(v, &ret, u)
        }
    }
}

#[derive(Debug, Clone)]
pub enum UnifyError {
    Mismatch { a: Ty, b: Ty },
    Infinite { var: TyVarId, ty: Ty },
    EffectMismatch { a: EffectSet, b: EffectSet },
}
