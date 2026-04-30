//! Bytecode instruction set per spec §8.2.

use serde::{Deserialize, Serialize};

/// Constant pool entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    Unit,
    /// A field name, used by MAKE_RECORD/GET_FIELD.
    FieldName(String),
    /// A variant tag, used by MAKE_VARIANT/TEST_VARIANT/GET_VARIANT.
    VariantName(String),
    /// An AST NodeId, attached to Call / EffectCall for trace keying (§10.1).
    NodeId(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Op {
    // stack manipulation
    PushConst(u32),
    Pop,
    Dup,

    // locals
    LoadLocal(u16),
    StoreLocal(u16),

    // constructors / pattern matching
    /// Builds a record from `count` (name_const_idx, value) pairs on the stack.
    /// Stack: [name_idx_n, val_n, ..., name_idx_1, val_1] but encoded as
    /// alternating `<name_idx_const_u32> <value popped from stack>` — for
    /// simplicity we instead push `count` values and `count` field name
    /// constants in the same op as a `Vec<u32>` of name indices.
    MakeRecord { field_name_indices: Vec<u32> },
    MakeTuple(u16),
    MakeList(u32),
    MakeVariant { name_idx: u32, arity: u16 },
    GetField(u32),         // field name const idx
    GetElem(u16),          // tuple element index
    TestVariant(u32),      // pushes Bool: top-of-stack matches variant name?
    GetVariant(u32),       // extracts payload (replaces variant on stack with its args list)
    GetVariantArg(u16),    // pop variant, push its i'th arg
    GetListLen,
    GetListElem(u32),

    // control flow
    Jump(i32),
    JumpIf(i32),     // pops Bool
    JumpIfNot(i32),
    Call { fn_id: u32, arity: u16, node_id_idx: u32 },
    TailCall { fn_id: u32, arity: u16, node_id_idx: u32 },
    /// EFFECT_CALL `<effect_kind_const_idx>` `<op_name_const_idx>` `<arity>`.
    /// Pops `arity` args, dispatches to a host effect handler, pushes result.
    /// `node_id_idx` points to a `Const::NodeId` for trace keying.
    EffectCall { kind_idx: u32, op_idx: u32, arity: u16, node_id_idx: u32 },
    Return,
    Panic(u32),     // pushes constant message and aborts

    // arithmetic — typed (per spec §8.2). `NumAdd`/etc. dispatch on operand
    // type at runtime; emitted when the compiler doesn't have type info.
    // The post-M5 plan is to lower NumAdd → IntAdd|FloatAdd in a typed pass.
    IntAdd, IntSub, IntMul, IntDiv, IntMod, IntNeg,
    IntEq, IntLt, IntLe,
    FloatAdd, FloatSub, FloatMul, FloatDiv, FloatNeg,
    FloatEq, FloatLt, FloatLe,
    NumAdd, NumSub, NumMul, NumDiv, NumMod, NumNeg,
    NumEq, NumLt, NumLe,
    BoolAnd, BoolOr, BoolNot,

    // strings
    StrConcat, StrLen, StrEq,
    BytesLen, BytesEq,
}
