//! Internal type representation used by the inferencer/checker.
//!
//! We model types as either ground constructors (Int, List[T], ...) or
//! unification variables. The `unifier` module handles solving.

use indexmap::IndexMap;
use std::collections::BTreeSet;

pub type TyVarId = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Var(TyVarId),
    Prim(Prim),
    Unit,
    Never,
    List(Box<Ty>),
    Tuple(Vec<Ty>),
    /// Sorted alphabetically by field name.
    Record(IndexMap<String, Ty>),
    /// e.g. `Result[Int, Str]` or `Option[T]`. Resolved against the type env.
    Con(String, Vec<Ty>),
    Function {
        params: Vec<Ty>,
        effects: EffectSet,
        ret: Box<Ty>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Prim { Int, Float, Bool, Str, Bytes }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectSet(pub BTreeSet<String>);

impl EffectSet {
    pub fn empty() -> Self { Self(BTreeSet::new()) }
    pub fn singleton(s: impl Into<String>) -> Self {
        let mut bs = BTreeSet::new();
        bs.insert(s.into());
        Self(bs)
    }
    pub fn union(&self, other: &EffectSet) -> EffectSet {
        EffectSet(self.0.union(&other.0).cloned().collect())
    }
    pub fn is_subset(&self, other: &EffectSet) -> bool { self.0.is_subset(&other.0) }
    pub fn extend(&mut self, other: &EffectSet) { self.0.extend(other.0.iter().cloned()); }
}

impl Ty {
    pub fn int() -> Self { Ty::Prim(Prim::Int) }
    pub fn float() -> Self { Ty::Prim(Prim::Float) }
    pub fn bool() -> Self { Ty::Prim(Prim::Bool) }
    pub fn str() -> Self { Ty::Prim(Prim::Str) }
    pub fn bytes() -> Self { Ty::Prim(Prim::Bytes) }
    pub fn function(params: Vec<Ty>, effects: EffectSet, ret: Ty) -> Self {
        Ty::Function { params, effects, ret: Box::new(ret) }
    }
    pub fn pretty(&self) -> String {
        match self {
            Ty::Var(v) => format!("?{}", v),
            Ty::Prim(p) => match p {
                Prim::Int => "Int", Prim::Float => "Float", Prim::Bool => "Bool",
                Prim::Str => "Str", Prim::Bytes => "Bytes",
            }.into(),
            Ty::Unit => "Unit".into(),
            Ty::Never => "Never".into(),
            Ty::List(t) => format!("List[{}]", t.pretty()),
            Ty::Tuple(items) => {
                let parts: Vec<_> = items.iter().map(|t| t.pretty()).collect();
                format!("({})", parts.join(", "))
            }
            Ty::Record(fields) => {
                let parts: Vec<_> = fields.iter().map(|(k, t)| format!("{} :: {}", k, t.pretty())).collect();
                format!("{{ {} }}", parts.join(", "))
            }
            Ty::Con(name, args) => {
                if args.is_empty() {
                    name.clone()
                } else {
                    let parts: Vec<_> = args.iter().map(|t| t.pretty()).collect();
                    format!("{}[{}]", name, parts.join(", "))
                }
            }
            Ty::Function { params, effects, ret } => {
                let parts: Vec<_> = params.iter().map(|t| t.pretty()).collect();
                let eff = if effects.0.is_empty() { String::new() } else {
                    let es: Vec<_> = effects.0.iter().cloned().collect();
                    format!("[{}] ", es.join(", "))
                };
                format!("({}) -> {}{}", parts.join(", "), eff, ret.pretty())
            }
        }
    }
}

/// A polymorphic scheme: type with universally-quantified type variables.
#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<TyVarId>,
    pub ty: Ty,
}
