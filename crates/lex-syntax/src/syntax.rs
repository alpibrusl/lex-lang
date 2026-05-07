//! Syntax tree (the parser's direct output, before canonicalization in §5).
//!
//! These nodes hew closely to source syntax; canonicalization (§5.3) folds
//! `if` into `Match`, `?` into its match expansion, etc.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Item {
    Import(Import),
    TypeDecl(TypeDecl),
    FnDecl(FnDecl),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Import {
    pub reference: String,
    pub alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeDecl {
    pub name: String,
    pub params: Vec<String>,
    pub definition: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub effects: Vec<Effect>,
    pub return_type: TypeExpr,
    pub body: Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Effect {
    pub name: String,
    pub arg: Option<EffectArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EffectArg {
    Str(String),
    Int(i64),
    Ident(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TypeExpr {
    /// Named primitive or constructor application (`Int`, `Result[Int, Str]`, `T`).
    /// We resolve which it is during type-checking, not parsing.
    Named { name: String, args: Vec<TypeExpr> },
    Record(Vec<TypeField>),
    Tuple(Vec<TypeExpr>),
    Function {
        params: Vec<TypeExpr>,
        effects: Vec<Effect>,
        ret: Box<TypeExpr>,
    },
    Union(Vec<UnionVariant>),
    /// Refinement type (#209): a base type plus a predicate the
    /// inhabitant must satisfy. `Int{x | x > 0 and x <= balance}`
    /// parses with `base = Named { name: "Int", args: [] }`,
    /// `binding = "x"`, and `predicate = (x > 0) and (x <= balance)`.
    /// Slice 1 stores the refinement; the type checker treats the
    /// refined type as its base. Slice 2 wires up static discharge
    /// via the spec-checker's gate evaluator; slice 3 adds the
    /// residual runtime check at call boundaries.
    Refined {
        base: Box<TypeExpr>,
        binding: String,
        predicate: Box<Expr>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeField {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnionVariant {
    pub name: String,
    /// `None` = tag-only (`Empty`); `Some(payload)` = constructor with payload.
    pub payload: Option<TypeExpr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub statements: Vec<Statement>,
    pub result: Box<Expr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Statement {
    Let { name: String, ty: Option<TypeExpr>, value: Expr },
    Expr(Expr),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Expr {
    Lit(Literal),
    Var(String),
    Block(Block),
    Call { callee: Box<Expr>, args: Vec<Expr> },
    Pipe { left: Box<Expr>, right: Box<Expr> },
    /// `expr?` postfix.
    Try(Box<Expr>),
    /// `expr.field`
    Field { value: Box<Expr>, field: String },
    BinOp { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    UnaryOp { op: UnaryOp, expr: Box<Expr> },
    If { cond: Box<Expr>, then_block: Block, else_block: Block },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm> },
    RecordLit(Vec<RecordLitField>),
    TupleLit(Vec<Expr>),
    ListLit(Vec<Expr>),
    /// A bare constructor name (`None`, `Empty`) or constructor call (`Ok(x)`).
    /// Since we cannot distinguish a constructor from a variable at parse
    /// time, the parser emits `Var`/`Call` and the type checker resolves it.
    /// This variant is kept for the canonicalizer to lift detected
    /// constructors into.
    Constructor { name: String, args: Vec<Expr> },
    Lambda(Box<Lambda>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lambda {
    pub params: Vec<Param>,
    pub return_type: TypeExpr,
    pub effects: Vec<Effect>,
    pub body: Block,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordLitField {
    pub name: String,
    pub value: Expr,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Neq, Lt, Lte, Gt, Gte,
    And, Or,
}

impl BinOp {
    pub fn precedence(self) -> u8 {
        use BinOp::*;
        match self {
            Or => 1,
            And => 2,
            Eq | Neq | Lt | Lte | Gt | Gte => 3,
            Add | Sub => 4,
            Mul | Div | Mod => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
            Eq => "==", Neq => "!=", Lt => "<", Lte => "<=", Gt => ">", Gte => ">=",
            And => "and", Or => "or",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum UnaryOp { Neg, Not }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Arm {
    pub pattern: Pattern,
    pub body: Expr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Pattern {
    Lit(Literal),
    /// Bare ident — either a binder or (during canonicalization) a tag-only constructor.
    Var(String),
    Wild,
    Constructor { name: String, args: Vec<Pattern> },
    Record { fields: Vec<RecordPatField>, rest: bool },
    Tuple(Vec<Pattern>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecordPatField {
    pub name: String,
    /// `None` means shorthand `{ name }` => `{ name: name }`.
    pub pattern: Option<Pattern>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Unit,
}
