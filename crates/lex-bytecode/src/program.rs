//! Compiled program: a set of functions plus a constant pool.

use crate::op::*;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Program {
    pub constants: Vec<Const>,
    pub functions: Vec<Function>,
    /// Global function names → function index in `functions`.
    pub function_names: IndexMap<String, u32>,
    /// Imported module aliases → module name (e.g., `io` → `io`).
    /// Used by the compiler/runtime to dispatch `alias.op(...)` calls.
    pub module_aliases: IndexMap<String, String>,
    /// Entry function (for `lex run`, set to whatever function the user
    /// chose to invoke). Optional.
    pub entry: Option<u32>,
    /// Interned record field-name shapes (#461). Each entry is a list
    /// of constant-pool indices (must point at `Const::FieldName`).
    /// `Op::MakeRecord { shape_idx, .. }` indexes into this side-table.
    /// Hoisting the field-name list out of the op stream is what
    /// lets `Op` be `Copy`.
    #[serde(default)]
    pub record_shapes: Vec<Vec<u32>>,
}

impl Program {
    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.function_names.get(name).copied()
    }

    /// Walk every function's declared effects and collect the union of
    /// effect kinds (with their args).
    pub fn declared_effects(&self) -> Vec<DeclaredEffect> {
        let mut out: Vec<DeclaredEffect> = Vec::new();
        for f in &self.functions {
            for e in &f.effects {
                if !out.iter().any(|x| x == e) {
                    out.push(e.clone());
                }
            }
        }
        out
    }
}

/// Content hash of a function body (#222). 16 bytes = SHA-256 truncated.
/// Matches `lex-vcs::OpId`'s width so that mixing the two never confuses a
/// reader expecting a uniform hash size across the codebase.
pub type BodyHash = [u8; 16];

/// All-zero sentinel — used in `Function::default()` and as a placeholder
/// before the hash is computed at the end of the compile pass.
pub const ZERO_BODY_HASH: BodyHash = [0u8; 16];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Function {
    pub name: String,
    pub arity: u16,
    pub locals_count: u16,
    pub code: Vec<Op>,
    /// Declared effects on this function's signature (spec §7).
    #[serde(default)]
    pub effects: Vec<DeclaredEffect>,
    /// Content hash of the bytecode body — see `compute_body_hash`.
    /// Populated at the end of the compile pass; used at `Op::MakeClosure`
    /// to give every `Value::Closure` a canonical identity that does not
    /// depend on the closure literal's source location (#222).
    #[serde(default = "zero_body_hash")]
    pub body_hash: BodyHash,
    /// Per-parameter refinement predicates (#209 slice 3). `Some(r)`
    /// for params declared with `Type{x | predicate}`, `None`
    /// otherwise. The VM evaluates these at `Op::Call` time before
    /// pushing the frame; failure raises `VmError::RefinementFailed`
    /// and the tracer records a verdict event with the same shape
    /// as a runtime gate's `gate.verdict`.
    #[serde(default)]
    pub refinements: Vec<Option<Refinement>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Refinement {
    /// The bound variable name from `Type{binding | predicate}`.
    pub binding: String,
    /// The predicate, stored as a canonical-AST `CExpr`. The VM
    /// interprets it directly via a small tree-walk evaluator —
    /// no separate compile pass needed since predicates are pure
    /// expressions over a single binding plus, eventually, the
    /// surrounding call-site context (slice 3 supports the
    /// binding only).
    pub predicate: lex_ast::CExpr,
}

fn zero_body_hash() -> BodyHash { ZERO_BODY_HASH }

/// Hash a function body so that two structurally-identical bodies — the
/// `fn(x) -> x + 1` literal repeated at two source locations, two flow
/// trampolines built from the same shape, etc. — yield the same hash.
///
/// Inputs: the bytecode `Op` sequence, the arity, the locals count.
/// Capture *types* are intentionally not hashed: capture *values* already
/// participate in `Value::Closure`'s equality through the `captures`
/// field, so two closures with different capture values already compare
/// non-equal regardless of the hash. Capture *types* without values
/// don't add equality information that captures don't already provide
/// (a value of type `Int` and a value of type `Str` can't both be `42`).
///
/// Constants pool indices referenced from the body are *not* resolved
/// before hashing — within a single compile the pool is shared, so two
/// equivalent literals produce identical `Op` sequences. Cross-compile
/// canonicality is deliberately out of scope (#222).
pub fn compute_body_hash(
    arity: u16,
    locals_count: u16,
    code: &[Op],
    record_shapes: &[Vec<u32>],
) -> BodyHash {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(arity.to_le_bytes());
    hasher.update(locals_count.to_le_bytes());
    hasher.update((code.len() as u64).to_le_bytes());
    // Serialize each Op deterministically. The serde-derived JSON form
    // is the canonical wire shape — closures with the same body must
    // hash identically across builds. `Op::MakeRecord` is special-cased:
    // its on-disk representation (a `shape_idx` into the side-table
    // plus a cached `field_count`) is rehydrated to the historical
    // inline form `{"MakeRecord":{"field_name_indices":[...]}}` so the
    // hash bytes stay bit-identical to pre-#461 builds.
    for op in code {
        let bytes = match op {
            Op::MakeRecord { shape_idx, .. } => {
                let shape = &record_shapes[*shape_idx as usize];
                #[derive(Serialize)]
                struct LegacyMakeRecord<'a> {
                    field_name_indices: &'a [u32],
                }
                #[derive(Serialize)]
                enum LegacyOp<'a> {
                    MakeRecord(LegacyMakeRecord<'a>),
                }
                serde_json::to_vec(&LegacyOp::MakeRecord(LegacyMakeRecord {
                    field_name_indices: shape,
                }))
                .expect("Op serialization must succeed")
            }
            _ => serde_json::to_vec(op).expect("Op serialization must succeed"),
        };
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let full = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeclaredEffect {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg: Option<EffectArg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EffectArg {
    Str(String),
    Int(i64),
    Ident(String),
}
