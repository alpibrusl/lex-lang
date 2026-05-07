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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecType {
    Int,
    Float,
    Bool,
    Str,
    /// Record type with named fields (#208). Quantifying over a
    /// record-shaped binding lets specs reference structured agent
    /// state without flattening into per-field scalar bindings.
    /// Fields are stored in declaration order; the gate evaluator
    /// resolves `expr.field` against `Value::Record`'s `IndexMap`,
    /// which preserves insertion order.
    Record { fields: Vec<(String, SpecType)> },
    /// List of an element type (#208). Quantifying over a list lets
    /// specs reason about agent collections — outstanding orders,
    /// active charging sessions, message queues — via `length`,
    /// `head`, `tail`, and indexed access (`xs[i]`).
    List { element: Box<SpecType> },
    /// Named user type (#208 slice 3). Refers to a user-defined ADT
    /// from the host program (e.g. `Message`, `Order`). The gate
    /// evaluator inspects the value's variant tag at match time;
    /// no compile-time variant table is needed for the gate path.
    /// The random-input prover (`check_spec`) can't sample arbitrary
    /// user types and fails out — those tests should provide
    /// concrete bindings instead.
    Named { name: String },
}

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
    /// Field access on a record-typed expression (#208). Evaluated by
    /// drilling into `Value::Record`'s field map; fails-loudly if the
    /// underlying value isn't a record or doesn't contain the field.
    FieldAccess { value: Box<SpecExpr>, field: String },
    /// Indexed access on a list-typed expression (#208). `xs[i]`
    /// evaluates to the i-th element of the list (zero-based).
    /// Out-of-bounds indices fail loudly via Inconclusive — agents
    /// that want defensive behavior wrap with a `length(xs) > i`
    /// check.
    Index { list: Box<SpecExpr>, index: Box<SpecExpr> },
    /// Pattern match on a sum-typed expression (#208 slice 3). Arms
    /// are tried in order; the first matching arm's body is the
    /// result. A `_` wildcard pattern is exhaustive. Variant
    /// patterns (`Charge(x)`) bind positional args by name in the
    /// arm's body. Non-exhaustive matches fall through to
    /// Inconclusive at evaluation time.
    Match { scrutinee: Box<SpecExpr>, arms: Vec<MatchArm> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MatchArm {
    pub pattern: SpecPattern,
    pub body: SpecExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpecPattern {
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// `Variant(x, y)` — matches `Value::Variant { name, args }`
    /// where `name == self.name` and `args.len() == bindings.len()`.
    /// Each binding name is bound to the corresponding positional
    /// arg in the arm's body. `Variant()` (no parens) and `Variant`
    /// (no args) parse identically; both have `bindings: vec![]`.
    Variant { name: String, bindings: Vec<String> },
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
