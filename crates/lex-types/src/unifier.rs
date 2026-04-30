//! Union-find based unification for type variables.

use crate::types::*;
use indexmap::IndexMap;

#[derive(Default)]
pub struct Unifier {
    next_var: TyVarId,
    /// Substitutions: `subst[v] = t` means var `v` was bound to `t`.
    subst: IndexMap<TyVarId, Ty>,
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
                effects: effects.clone(),
                ret: Box::new(self.resolve(ret)),
            },
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
                if p1.len() == p2.len() && e1 == e2 =>
            {
                for (x, y) in p1.iter().zip(p2.iter()) { self.unify(x, y)?; }
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
}
