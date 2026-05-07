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

/// Effect set with an optional row variable.
///
/// `concrete` is the closed lower bound — effect kinds the function
/// definitely uses. `var` is an open extension point used for effect
/// polymorphism on stdlib higher-order functions: `list.map[T, U, E]`
/// declares its callback as `(T) -> [E] U` where `E` is `var: Some(id)`.
/// At call sites the variable unifies with whatever effect set the
/// actual closure carries, then propagates to the result.
///
/// All call sites that compare or merge concrete-only sets continue to
/// work via the helper methods, which delegate to `concrete`.
/// A single effect entry. Bare `[net]` is the wildcard form
/// (`arg = None`); parameterized `[net("wttr.in")]` carries an arg
/// (#207). Sort order matches the `String` ordering of `(name, arg)`
/// so `BTreeSet<EffectKind>` keeps a canonical iteration order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EffectKind {
    pub name: String,
    pub arg: Option<EffectArg>,
}

/// Mirror of `lex-ast::EffectArg`. Type-level effects use this so
/// the checker, lex-vcs ChangeEffectSig, and the runtime gate all
/// reason about the same shape (#207).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EffectArg {
    Str(String),
    Int(i64),
    Ident(String),
}

impl EffectKind {
    /// Bare `[name]` — the wildcard form. Matches anything with the
    /// same name regardless of arg.
    pub fn bare(name: impl Into<String>) -> Self {
        Self { name: name.into(), arg: None }
    }
    /// Parameterized `[name("value")]`.
    pub fn with_str(name: impl Into<String>, arg: impl Into<String>) -> Self {
        Self { name: name.into(), arg: Some(EffectArg::Str(arg.into())) }
    }
    /// Returns true iff `self` is at least as permissive as `other` —
    /// i.e., a context declaring `self` can satisfy a callee
    /// requiring `other`. Bare absorbs specific (`[mcp]` accepts
    /// `[mcp(ocpp)]`); specifics match only themselves (`[mcp(ocpp)]`
    /// does not accept `[mcp(other)]`); names must match either way.
    pub fn subsumes(&self, other: &EffectKind) -> bool {
        if self.name != other.name { return false; }
        match (&self.arg, &other.arg) {
            (None, _) => true,             // bare wildcard absorbs anything
            (Some(_), None) => false,      // specific can't grant bare
            (Some(a), Some(b)) => a == b,  // specifics match only themselves
        }
    }
    /// Render for diagnostics. `[name]` for bare, `[name("arg")]`
    /// for string args, `[name(arg)]` for int/ident.
    pub fn pretty(&self) -> String {
        match &self.arg {
            None => self.name.clone(),
            Some(EffectArg::Str(s)) => format!("{}(\"{}\")", self.name, s),
            Some(EffectArg::Int(n)) => format!("{}({})", self.name, n),
            Some(EffectArg::Ident(s)) => format!("{}({})", self.name, s),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectSet {
    pub concrete: BTreeSet<EffectKind>,
    pub var: Option<u32>,
}

impl EffectSet {
    pub fn empty() -> Self { Self::default() }
    /// Backwards-compatible constructor — produces a bare `[name]`
    /// effect. New code that wants parameterized effects builds an
    /// `EffectKind` directly and inserts it into `concrete`, or uses
    /// `EffectSet::singleton_arg`.
    pub fn singleton(s: impl Into<String>) -> Self {
        let mut bs = BTreeSet::new();
        bs.insert(EffectKind::bare(s));
        Self { concrete: bs, var: None }
    }
    /// Parameterized singleton, e.g. `[net("wttr.in")]`.
    pub fn singleton_arg(name: impl Into<String>, arg: impl Into<String>) -> Self {
        let mut bs = BTreeSet::new();
        bs.insert(EffectKind::with_str(name, arg));
        Self { concrete: bs, var: None }
    }
    /// An open effect set: just a row variable, no concrete lower
    /// bound. Used by stdlib HOF signatures (list.map, list.filter,
    /// list.fold, option.map, result.map, result.and_then).
    pub fn open_var(id: u32) -> Self {
        Self { concrete: BTreeSet::new(), var: Some(id) }
    }
    pub fn union(&self, other: &EffectSet) -> EffectSet {
        EffectSet {
            concrete: self.concrete.union(&other.concrete).cloned().collect(),
            var: self.var.or(other.var),
        }
    }
    /// `self` is a subset of `other` iff every entry in `self` is
    /// subsumed by *some* entry in `other`. Per #207 this honors
    /// bare-wildcard absorption: `{[mcp(ocpp)]} ⊆ {[mcp]}` holds.
    pub fn is_subset(&self, other: &EffectSet) -> bool {
        self.concrete.iter().all(|need| {
            other.concrete.iter().any(|have| have.subsumes(need))
        })
    }
    pub fn extend(&mut self, other: &EffectSet) {
        self.concrete.extend(other.concrete.iter().cloned());
        if self.var.is_none() { self.var = other.var; }
    }
    pub fn is_open(&self) -> bool { self.var.is_some() }
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
                let eff = if effects.concrete.is_empty() && effects.var.is_none() {
                    String::new()
                } else {
                    let mut es: Vec<String> = effects.concrete.iter().map(EffectKind::pretty).collect();
                    if let Some(v) = effects.var { es.push(format!("?e{}", v)); }
                    format!("[{}] ", es.join(", "))
                };
                format!("({}) -> {}{}", parts.join(", "), eff, ret.pretty())
            }
        }
    }
}

/// A polymorphic scheme: type with universally-quantified type
/// variables and effect-row variables.
#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<TyVarId>,
    pub eff_vars: Vec<u32>,
    pub ty: Ty,
}
