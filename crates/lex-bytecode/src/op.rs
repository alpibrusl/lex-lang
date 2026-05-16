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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Op {
    // stack manipulation
    PushConst(u32),
    Pop,
    Dup,

    // locals
    LoadLocal(u16),
    StoreLocal(u16),

    // constructors / pattern matching
    /// Builds a record by interning its field-name shape in
    /// `Program.record_shapes` (#461). `shape_idx` indexes that
    /// side-table; `field_count` is `shape.len()` cached inline so the
    /// stack-effect verifier can compute its delta without needing a
    /// `Program` reference. The VM pops `field_count` values off the
    /// stack and pairs them with `Program.record_shapes[shape_idx]`.
    ///
    /// Externalizing the field-name vec is what lets `Op` be `Copy`,
    /// which is the precondition for direct-threaded dispatch
    /// (`code[pc]` becomes a register-sized read instead of an
    /// every-step `Vec` clone).
    MakeRecord { shape_idx: u32, field_count: u16 },
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
    /// Pop [list, value]; push list with `value` appended.
    ListAppend,
    /// Pop list; push it indexed by the integer on top.
    /// Stack: [list, idx] → [list[idx]]. (Like GetListElem(u32) but
    /// the index is dynamic.)
    GetListElemDyn,

    // control flow
    Jump(i32),
    JumpIf(i32),     // pops Bool
    JumpIfNot(i32),
    Call { fn_id: u32, arity: u16, node_id_idx: u32 },
    TailCall { fn_id: u32, arity: u16, node_id_idx: u32 },
    /// Build a Value::Closure: pop `capture_count` values (in order) and
    /// pair them with `fn_id`.
    MakeClosure { fn_id: u32, capture_count: u16 },
    /// Call a closure: pop `arity` args + 1 closure (top of stack), invoke.
    CallClosure { arity: u16, node_id_idx: u32 },
    /// Stable sort-by-key (#338). Stack: `[xs, f]` (xs underneath).
    /// Pops the key-fn `f` and the list `xs`, applies `f` to each
    /// element to derive a sortable key, returns the list reordered
    /// so keys ascend. Keys must be one of `Int` / `Float` / `Str`;
    /// other key types pair-wise compare as equal (preserving
    /// insertion order). `node_id_idx` is the originating NodeId.
    SortByKey { node_id_idx: u32 },
    /// Parallel map (#305 slice 1). Stack: `[xs, f]` (xs underneath).
    /// Pops the closure `f` and the list `xs`, applies `f` to each
    /// element in parallel via OS threads, pushes the result list
    /// in input order. `node_id_idx` is the originating NodeId for
    /// trace keying. The pool size is capped by
    /// `LEX_PAR_MAX_CONCURRENCY` (default = available CPU cores).
    ///
    /// Slice 1 limitation: closures invoking effects fail at
    /// runtime with `VmError::Effect`. The per-thread effect handler
    /// split is queued as slice 2.
    ParallelMap { node_id_idx: u32 },
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
