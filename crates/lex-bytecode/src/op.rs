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
    /// Record field access. `name_idx` indexes a `Const::FieldName`
    /// in the constant pool — the field to read. `site_idx` is a
    /// stable per-function index assigned by the compiler at emit
    /// time (#462 slice 1), keyed into the per-fn inline-cache table
    /// in the VM. Replaces the pre-#462 `(fn_id << 32 | pc)` IC key
    /// so the cache survives the future dispatch rewrite (#461) and
    /// a JIT (#465). `body_hash` stability: the canonical encoding
    /// drops `site_idx` and serializes as the historical `GetField(u32)`
    /// tuple form, so closure identity (#222) is unchanged.
    GetField { name_idx: u32, site_idx: u32 },
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

    // superinstructions (#461)
    //
    // Fused opcodes emitted by the compiler's peephole pass to skip
    // dispatch on common multi-op patterns. The pass leaves the
    // original primitive ops in place at the trailing slots — the
    // dispatch loop overrides its default `pc += 1` to step past
    // them. Keeping `code.len()` invariant means existing
    // Jump/JumpIf offsets remain valid without a renumbering pass.
    /// Fused `LoadLocal(local_idx) + PushConst(imm_const_idx) +
    /// IntAdd`. `imm_const_idx` must point to a `Const::Int`. The
    /// dispatch arm reads the local, adds the constant, pushes the
    /// result, and advances pc by 3 (past this op and the two
    /// inert PushConst + IntAdd slots that follow). For
    /// `body_hash` stability (#222) the canonical encoding decomposes
    /// this op back to a standalone `LoadLocal(local_idx)` at hash
    /// time; the unchanged PushConst / IntAdd at the next two
    /// slots hash normally, so the total bytes match pre-fusion.
    LoadLocalAddIntConst { local_idx: u16, imm_const_idx: u32 },
    /// Fused `LoadLocal(src) + PushConst(imm_const_idx) + IntAdd +
    /// StoreLocal(dest)` (#461 superinstruction slice 2). Bypasses
    /// the value stack entirely: reads `locals[src]`, adds the Int
    /// constant, writes `locals[dest]`. Advances pc by 4. Stack
    /// delta: 0.
    ///
    /// The peephole pass that emits this op runs *after* slice 1,
    /// looking for `[LoadLocalAddIntConst, ., ., StoreLocal]` where
    /// the middle two slots are slice-1 tombstones (the original
    /// PushConst + IntAdd). The verifier and the body-hash decoder
    /// both treat the 3 following slots as tombstones owned by
    /// this op.
    LoadLocalAddIntConstStoreLocal { src: u16, imm_const_idx: u32, dest: u16 },
    /// Fused `LoadLocal(lhs_idx) + LoadLocal(rhs_idx) + IntAdd`
    /// (#461 superinstruction slice 3). The binary-op-on-two-locals
    /// idiom — fires on any `a + b` where both operands are
    /// statically-typed `Int` locals (e.g. `acc + n` in tail-recursive
    /// accumulator loops). Reads `locals[lhs_idx]` and `locals[rhs_idx]`,
    /// pushes the sum, advances pc by 3. Stack delta: +1.
    ///
    /// `body_hash` stability (#222): canonical encoding decomposes
    /// back to a standalone `LoadLocal(lhs_idx)`. The unchanged
    /// `LoadLocal(rhs_idx)` + `IntAdd` tombstones at pc+1 and pc+2
    /// hash normally, so the total bytes match the pre-fusion form.
    /// Verifier walks the tombstones as if live: their deltas
    /// (+1 LoadLocal, -1 IntAdd) cancel, matching the unfused depth
    /// at pc+3.
    LoadLocalAddLocal { lhs_idx: u16, rhs_idx: u16 },
    /// Fused `LoadLocal(lhs_idx) + LoadLocal(rhs_idx) + IntSub`
    /// (#461 superinstruction slice 4). Sibling of `LoadLocalAddLocal`
    /// for the typed `Int` subtraction binop — fires on any `a - b`
    /// where both operands are `Int` locals. Reads `locals[lhs_idx]`
    /// and `locals[rhs_idx]`, pushes `lhs - rhs`, advances pc by 3.
    /// Stack delta: +1. Tombstone + body-hash story matches
    /// `LoadLocalAddLocal` exactly.
    LoadLocalSubLocal { lhs_idx: u16, rhs_idx: u16 },
    /// Fused `LoadLocal(lhs_idx) + LoadLocal(rhs_idx) + IntMul`
    /// (#461 superinstruction slice 4). Sibling of `LoadLocalAddLocal`
    /// for the typed `Int` multiplication binop. Same shape: reads
    /// the two Int locals, pushes `lhs * rhs`, advances pc by 3.
    /// Stack delta: +1. Tombstone + body-hash story matches
    /// `LoadLocalAddLocal` exactly.
    LoadLocalMulLocal { lhs_idx: u16, rhs_idx: u16 },
}
