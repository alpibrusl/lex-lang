//! Canonical AST per spec §5.1.
//!
//! Two views:
//! - The data tree itself (this module).
//! - Node-IDs computed from positions (§5.2; see [`node_id`]).
//!
//! The canonical AST has no `If`, no `?`, no parens, no comments. Record
//! field orders are sorted alphabetically; union variants too. The
//! canonicalizer (§5.3) is in `crate::canonicalize`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A whole stage's canonical form. Lex source is a *projection* of a set of
/// stages (§3.12); each `fn` and `type` declaration becomes one stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum Stage {
    FnDecl(FnDecl),
    TypeDecl(TypeDecl),
    Import(Import),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub effects: Vec<Effect>,
    pub return_type: TypeExpr,
    pub body: CExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeDecl {
    pub name: String,
    pub params: Vec<String>,
    pub definition: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Import {
    pub reference: String,
    pub alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Effect {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg: Option<EffectArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum EffectArg {
    Str { value: String },
    Int { value: i64 },
    Ident { value: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum TypeExpr {
    Named { name: String, args: Vec<TypeExpr> },
    Record { fields: Vec<TypeField> },
    Tuple { items: Vec<TypeExpr> },
    Function { params: Vec<TypeExpr>, effects: Vec<Effect>, ret: Box<TypeExpr> },
    Union { variants: Vec<UnionVariant> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnionVariant {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<TypeExpr>,
}

/// Canonical expression. No `If`, no `Try`. `Pipe` is normalized to `Call`
/// (`x |> f` ≡ `Call f [x]`, `x |> f(a)` ≡ `Call f [x, a]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum CExpr {
    Literal { value: CLit },
    Var { name: String },
    Call { callee: Box<CExpr>, args: Vec<CExpr> },
    Let { name: String, ty: Option<TypeExpr>, value: Box<CExpr>, body: Box<CExpr> },
    Match { scrutinee: Box<CExpr>, arms: Vec<Arm> },
    Block { statements: Vec<CExpr>, result: Box<CExpr> },
    Constructor { name: String, args: Vec<CExpr> },
    RecordLit { fields: Vec<RecordField> },
    TupleLit { items: Vec<CExpr> },
    ListLit { items: Vec<CExpr> },
    FieldAccess { value: Box<CExpr>, field: String },
    Lambda { params: Vec<Param>, return_type: TypeExpr, effects: Vec<Effect>, body: Box<CExpr> },
    BinOp { op: String, lhs: Box<CExpr>, rhs: Box<CExpr> },
    UnaryOp { op: String, expr: Box<CExpr> },
    /// Tail position only; used for `?` desugaring.
    Return { value: Box<CExpr> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordField {
    pub name: String,
    pub value: CExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum CLit {
    Int { value: i64 },
    Float { value: String }, // Stringified for stable canonical encoding.
    Str { value: String },
    Bytes { value: String }, // Hex-encoded for stable JSON.
    Bool { value: bool },
    Unit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: CExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum Pattern {
    PLiteral { value: CLit },
    PVar { name: String },
    PWild,
    PConstructor { name: String, args: Vec<Pattern> },
    PRecord { fields: Vec<PatternRecordField> },
    PTuple { items: Vec<Pattern> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PatternRecordField {
    pub name: String,
    pub pattern: Pattern,
}

/// Indexed view (used by tools). Maps NodeId → reference into a stage.
/// Computing this is `O(n)` walk of the tree; we keep IDs out of the data.
pub struct WithIds<'a> {
    #[allow(dead_code)]
    pub root: &'a Stage,
    pub map: IndexMap<String, NodePath>,
}

#[derive(Debug, Clone)]
pub struct NodePath(pub Vec<usize>);

impl NodePath {
    pub fn id(&self) -> String {
        let mut s = String::from("n_0");
        for &p in &self.0 {
            s.push('.');
            s.push_str(&p.to_string());
        }
        s
    }
}
