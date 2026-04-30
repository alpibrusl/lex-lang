//! Spec AST.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    /// Spec name (typically the target function's name).
    pub name: String,
    pub quantifiers: Vec<Quantifier>,
    /// The boolean property. Free variables refer to quantifiers.
    pub body: SpecExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Quantifier {
    pub name: String,
    pub ty: SpecType,
    /// Optional `where` predicate restricting the quantifier domain.
    /// Variables in scope are previously-declared quantifiers and `name` itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<SpecExpr>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpecType { Int, Float, Bool, Str }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum SpecExpr {
    Var { name: String },
    IntLit { value: i64 },
    FloatLit { value: f64 },
    BoolLit { value: bool },
    StrLit { value: String },
    /// A call into the target Lex function (or another helper).
    Call { func: String, args: Vec<SpecExpr> },
    Let { name: String, value: Box<SpecExpr>, body: Box<SpecExpr> },
    BinOp { op: SpecOp, lhs: Box<SpecExpr>, rhs: Box<SpecExpr> },
    Not { expr: Box<SpecExpr> },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpecOp {
    Add, Sub, Mul, Div, Mod,
    Eq, Neq, Lt, Le, Gt, Ge,
    And, Or,
}

impl SpecOp {
    pub fn as_str(self) -> &'static str {
        use SpecOp::*;
        match self {
            Add => "+", Sub => "-", Mul => "*", Div => "/", Mod => "%",
            Eq => "==", Neq => "!=", Lt => "<", Le => "<=", Gt => ">", Ge => ">=",
            And => "and", Or => "or",
        }
    }
    pub fn is_arith(self) -> bool {
        use SpecOp::*; matches!(self, Add | Sub | Mul | Div | Mod)
    }
    pub fn is_compare(self) -> bool {
        use SpecOp::*; matches!(self, Eq | Neq | Lt | Le | Gt | Ge)
    }
    pub fn is_bool(self) -> bool {
        use SpecOp::*; matches!(self, And | Or)
    }
}
