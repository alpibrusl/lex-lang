//! The interpreter loop: `Vm::run_to` with its `Op` match, plus the
//! stack and arithmetic helpers it uses on the hot path. Frames,
//! the memo cache, effect dispatch and the host-facing entry points
//! stay in `super`; the superinstruction shapes it decodes are
//! produced by `crate::compiler::peephole`.

use super::*;

impl<'a> Vm<'a> {
    /// Run until the frame stack drops to `base_depth`. Required for
    /// reentrant invocation: a `Vm::invoke` call from inside an
    /// already-running `run()` must return when *its* frame returns,
    /// not when the entire frame stack empties (#221).
    pub(super) fn run_to(&mut self, base_depth: usize) -> Result<Value, VmError> {
        // #461 slice A: cache the executing function's code slice across
        // ops instead of re-deriving `program.functions[fn_id].code` on
        // every iteration. The program is borrowed (`&'a Program`) and is
        // never mutated during a run, so the slice reference is valid for
        // the whole run and — crucially — is independent of the `&mut self`
        // borrow the op handlers take: it points into the caller-owned
        // `Program`, not into `*self`. Re-resolve only when `fn_id`
        // changes, which is exactly the frame-transition set (Call /
        // CallClosure / TailCall / Return); recursion into the same
        // `fn_id` correctly keeps the cached slice. `frame_idx` / `fn_id`
        // stay recomputed per op (cheap field reads), so the op handlers
        // are untouched and their `fn_id` bindings shadow as before.
        let program: &'a Program = self.program;
        let mut code: &'a [Op] = &[];
        let mut code_fn_id: u32 = u32::MAX;
        loop {
            if self.steps > self.step_limit {
                let frame_idx = self.frames.len() - 1;
                let fn_id = self.frames[frame_idx].fn_id;
                let fn_name = &program.functions[fn_id as usize].name;
                return Err(VmError::Panic(format!(
                    "step limit exceeded in `{fn_name}` ({} > {})",
                    self.steps, self.step_limit,
                )));
            }
            self.steps += 1;
            let frame_idx = self.frames.len() - 1;
            let pc = self.frames[frame_idx].pc;
            let fn_id = self.frames[frame_idx].fn_id;
            if fn_id != code_fn_id {
                code = &program.functions[fn_id as usize].code;
                code_fn_id = fn_id;
            }
            // #461 slice B: the bytecode verifier (#366) proves pc stays
            // in bounds for every reachable op — every path through a
            // function ends in Return / Jump / TailCall, so execution
            // never falls off the end of `code`. The per-op
            // `pc >= code.len()` guard is therefore redundant for verified
            // programs; demote it to a debug-only assertion. The `code[pc]`
            // index below stays bounds-checked, so a malformed program in
            // a release build still panics (loudly, just without the
            // bespoke message) rather than reading out of bounds — no
            // `unsafe`, no UB, only the cold error-return path leaves the
            // hot loop.
            debug_assert!(
                pc < code.len(),
                "ran past end of code in `{}`",
                program.functions[fn_id as usize].name,
            );
            let op = code[pc];
            self.frames[frame_idx].pc = pc + 1;

            match op {
                Op::PushConst(i) => {
                    let c = &self.program.constants[i as usize];
                    self.stack.push(const_to_value(c));
                }
                Op::Pop => { self.pop()?; }
                Op::Dup => {
                    let v = self.peek()?.clone();
                    self.stack.push(v);
                }
                Op::LoadLocal(i) => {
                    let base = self.frames[frame_idx].locals_start;
                    let v = self.locals_storage[base + i as usize].clone();
                    self.stack.push(v);
                }
                Op::TakeLocal(i) => {
                    // Last read of this slot in the body (#774): move,
                    // don't clone, so the value reaches its consumer
                    // uniquely owned.
                    let base = self.frames[frame_idx].locals_start;
                    let v = std::mem::replace(
                        &mut self.locals_storage[base + i as usize],
                        Value::Unit,
                    );
                    self.stack.push(v);
                }
                Op::StoreLocal(i) => {
                    let v = self.pop()?;
                    let base = self.frames[frame_idx].locals_start;
                    self.locals_storage[base + i as usize] = v;
                }
                Op::MakeRecord { shape_idx, field_count } => {
                    self.heap_record_allocs += 1;
                    let shape = &self.program.record_shapes[shape_idx as usize];
                    let n = field_count as usize;
                    debug_assert_eq!(shape.len(), n,
                        "MakeRecord field_count must match record_shapes[shape_idx].len()");
                    let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n).rev() {
                        values[i] = self.pop()?;
                    }
                    let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                    for (i, val) in values.into_iter().enumerate() {
                        let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                            Const::FieldName(s) => s.as_str().into(),
                            _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                        };
                        rec.insert(name, val);
                    }
                    self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                }
                Op::AllocStackRecord { shape_idx, field_count } => {
                    // #464 step 2. Same value-stack contract as
                    // MakeRecord (pop `field_count`, push 1), but the
                    // fields live in the VM's stack-record arena
                    // instead of a heap-allocated IndexMap.
                    //
                    // Budget check: if this frame's remaining
                    // allocation budget can't cover `field_count`
                    // slots, fall back to MakeRecord behavior. The
                    // observable result is identical (a record
                    // value) so the compiler doesn't need to know
                    // ahead of time whether the budget will hold.
                    let n = field_count as usize;
                    let frame = &mut self.frames[frame_idx];
                    if frame.stack_record_budget_remaining < field_count as u32 {
                        self.stack_record_heap_fallbacks += 1;
                        // Heap fallback path — exact copy of
                        // MakeRecord's body. Compiler emitted
                        // AllocStackRecord because escape analysis
                        // proved the record can stay frame-local;
                        // the budget exhaustion is a runtime cost
                        // ceiling, not a correctness issue.
                        let shape = &self.program.record_shapes[shape_idx as usize];
                        debug_assert_eq!(shape.len(), n,
                            "AllocStackRecord field_count must match record_shapes[shape_idx].len()");
                        let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() {
                            values[i] = self.pop()?;
                        }
                        let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                        for (i, val) in values.into_iter().enumerate() {
                            let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                                Const::FieldName(s) => s.as_str().into(),
                                _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                            };
                            rec.insert(name, val);
                        }
                        self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                    } else {
                        self.stack_record_allocs += 1;
                        // Stack path: append the popped field values
                        // to the arena in shape order (matches the
                        // IndexMap insertion order used by MakeRecord,
                        // so the polymorphic GetField IC sees the same
                        // offset for either variant).
                        frame.stack_record_budget_remaining -= field_count as u32;
                        let slab_start = self.stack_record_arena.len();
                        // Reserve all slots upfront so we can write in
                        // shape order while popping in reverse —
                        // matches MakeRecord's idiom.
                        self.stack_record_arena.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.stack_record_arena[slab_start + i] = v;
                        }
                        self.stack.push(Value::StackRecord {
                            shape_id: shape_idx,
                            slab_start: slab_start as u32,
                            field_count,
                        });
                    }
                }
                Op::AllocArenaRecord { shape_idx, field_count } => {
                    // #463 slice 2a. Same value-stack contract as
                    // MakeRecord, but field values land in the
                    // request-scoped `arena_slab` instead of a
                    // per-field heap IndexMap. Runtime fallback when
                    // no scope is active — the op silently degrades
                    // to the MakeRecord heap path so arena-lowered
                    // bytecode stays sound in non-handler contexts
                    // (REPL, tests, top-level scripts).
                    let n = field_count as usize;
                    if self.arena_scope_starts.is_empty() {
                        self.arena_record_heap_fallbacks += 1;
                        // Heap fallback path — exact copy of
                        // MakeRecord's body. Same compile-time
                        // contract (shape order, IndexMap insertion)
                        // so the resulting Value::Record is
                        // indistinguishable from a direct MakeRecord.
                        let shape = &self.program.record_shapes[shape_idx as usize];
                        debug_assert_eq!(shape.len(), n,
                            "AllocArenaRecord field_count must match record_shapes[shape_idx].len()");
                        let mut values: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() {
                            values[i] = self.pop()?;
                        }
                        let mut rec: IndexMap<SmolStr, Value> = IndexMap::with_capacity(n);
                        for (i, val) in values.into_iter().enumerate() {
                            let name: SmolStr = match &self.program.constants[shape[i] as usize] {
                                Const::FieldName(s) => s.as_str().into(),
                                _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
                            };
                            rec.insert(name, val);
                        }
                        self.stack.push(Value::Record { shape_id: shape_idx, fields: Box::new(rec) });
                    } else {
                        self.arena_record_allocs += 1;
                        // Arena path: append the popped field values
                        // to the slab in shape order (matches
                        // MakeRecord's IndexMap insertion order, so
                        // the polymorphic GetField IC sees the same
                        // offset across all three variants).
                        let slab_start = self.arena_slab.len();
                        self.arena_slab.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.arena_slab[slab_start + i] = v;
                        }
                        self.stack.push(Value::ArenaRecord {
                            shape_id: shape_idx,
                            slab_start: slab_start as u32,
                            field_count,
                        });
                    }
                }
                Op::MakeTuple(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::Tuple(items));
                }
                Op::AllocStackTuple { arity } => {
                    // #464 tuple codegen. Same value-stack contract as
                    // MakeTuple (pop `arity`, push 1), but the elements
                    // live in the shared stack-record arena instead of
                    // a heap Vec. Budget exhaustion falls back to the
                    // MakeTuple heap path — identical observable result.
                    let n = arity as usize;
                    let frame = &mut self.frames[frame_idx];
                    if frame.stack_record_budget_remaining < arity as u32 {
                        self.stack_record_heap_fallbacks += 1;
                        let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() { items[i] = self.pop()?; }
                        self.stack.push(Value::Tuple(items));
                    } else {
                        self.stack_record_allocs += 1;
                        frame.stack_record_budget_remaining -= arity as u32;
                        let slab_start = self.stack_record_arena.len();
                        self.stack_record_arena.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.stack_record_arena[slab_start + i] = v;
                        }
                        self.stack.push(Value::StackTuple {
                            slab_start: slab_start as u32,
                            arity,
                        });
                    }
                }
                Op::AllocArenaTuple { arity } => {
                    // #463 slice 2a. Tuple analogue of
                    // AllocArenaRecord: arena slab when a scope is
                    // active, MakeTuple heap fallback otherwise.
                    let n = arity as usize;
                    if self.arena_scope_starts.is_empty() {
                        self.arena_record_heap_fallbacks += 1;
                        let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                        for i in (0..n).rev() { items[i] = self.pop()?; }
                        self.stack.push(Value::Tuple(items));
                    } else {
                        self.arena_record_allocs += 1;
                        let slab_start = self.arena_slab.len();
                        self.arena_slab.resize(slab_start + n, Value::Unit);
                        for i in (0..n).rev() {
                            let v = self.pop()?;
                            self.arena_slab[slab_start + i] = v;
                        }
                        self.stack.push(Value::ArenaTuple {
                            slab_start: slab_start as u32,
                            arity,
                        });
                    }
                }
                Op::MakeList(n) => {
                    let mut items: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n as usize).rev() { items[i] = self.pop()?; }
                    self.stack.push(Value::List(items.into()));
                }
                Op::MakeVariant { name_idx, arity } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    let name = match &self.program.constants[name_idx as usize] {
                        Const::VariantName(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected VariantName const".into())),
                    };
                    self.stack.push(Value::Variant { name, args });
                }
                Op::GetField { name_idx, site_idx } => {
                    let v = self.pop()?;
                    match v {
                        Value::Record { fields: r, shape_id } => {
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            // Inline cache keyed by (fn_id, site_idx) with
                            // shape_id-keyed verification (#462). Slot stores
                            // (shape_id_at_install, offset). Hit verification:
                            // - real shape_id (!= NO_SHAPE_ID) matches: offset
                            //   is guaranteed valid (records with the same
                            //   shape_id share the same field-name ordering
                            //   from the compile-time `record_shapes` table).
                            //   One u32 compare; no string work.
                            // - NO_SHAPE_ID matches NO_SHAPE_ID: distinct
                            //   dynamic shapes both carry this sentinel and
                            //   would otherwise alias, so we fall back to
                            //   verifying via name compare against the field
                            //   at the cached offset — the pre-slice
                            //   correctness path.
                            // On any mismatch we walk by name and reinstall
                            // (shape_id, offset).
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id {
                                        if shape_id != crate::value::NO_SHAPE_ID {
                                            // Real shape match: offset is sound.
                                            if let Some((_, val)) = r.get_index(off) {
                                                break 'ic val.clone();
                                            }
                                        } else if let Some((k, val)) = r.get_index(off) {
                                            // Dynamic shape: verify by name.
                                            if let Const::FieldName(s) =
                                                &self.program.constants[name_idx as usize]
                                            {
                                                if s == k { break 'ic val.clone(); }
                                            }
                                        }
                                    }
                                }
                                // Cache miss: resolve by name, install
                                // (shape_id, offset).
                                let name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let (off, _, val) = r.get_full(name)
                                    .ok_or_else(|| VmError::TypeMismatch(
                                        format!("missing field `{name}`")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                val.clone()
                            };
                            self.stack.push(value);
                        }
                        Value::StackRecord { shape_id, slab_start, field_count } => {
                            // #464 step 2: dispatch over a stack-allocated
                            // record. The IC slot stored
                            // (shape_id, offset_in_shape) is interoperable
                            // with the heap path because MakeRecord builds
                            // the IndexMap in shape order — offset N means
                            // the same field in either representation. So
                            // we share `field_ics` with the heap path; no
                            // per-variant cache pollution.
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id && (off as u16) < field_count {
                                        // Shape-keyed verification is sound
                                        // here for the same reason as the
                                        // heap path — compile-time shape
                                        // IDs are issued by
                                        // `Program::record_shapes` and
                                        // their field order is fixed.
                                        // Stack records always carry a
                                        // compile-time shape_id (NO_SHAPE_ID
                                        // is impossible — AllocStackRecord
                                        // is only emitted at compile time
                                        // with a known shape_idx).
                                        let idx = slab_start as usize + off;
                                        break 'ic self.stack_record_arena[idx].clone();
                                    }
                                }
                                // Cache miss: walk the shape's field-name
                                // vec to find the slot for `name_idx`. The
                                // miss path is O(field_count) like the
                                // heap path, but the hot retrieval after
                                // install is one array index — cheaper
                                // than IndexMap::get_index.
                                let shape =
                                    &self.program.record_shapes[shape_id as usize];
                                let target_name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let mut found: Option<usize> = None;
                                for (i, fn_const_idx) in shape.iter().enumerate() {
                                    if let Const::FieldName(s) =
                                        &self.program.constants[*fn_const_idx as usize]
                                    {
                                        if s == target_name { found = Some(i); break; }
                                    }
                                }
                                let off = found.ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{target_name}` on stack record")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                self.stack_record_arena[slab_start as usize + off].clone()
                            };
                            self.stack.push(value);
                        }
                        Value::ArenaRecord { shape_id, slab_start, field_count } => {
                            // #463 slice 2a: dispatch over an
                            // arena-allocated record. Identical IC
                            // story to `StackRecord` above — the slot
                            // stores `(shape_id, offset)` and offset
                            // semantics match `Value::Record`'s
                            // IndexMap insertion order, so the IC is
                            // three-way interoperable.
                            if ic_stats_enabled() {
                                record_ic_hit(fn_id, site_idx, shape_id);
                            }
                            let fid = fn_id as usize;
                            let sid = site_idx as usize;
                            if self.field_ics[fid].is_empty() {
                                let n = self.program.functions[fid].field_ic_sites as usize;
                                self.field_ics[fid] = vec![None; n];
                            }
                            let cached = self.field_ics[fid][sid];
                            let value = 'ic: {
                                if let Some((cached_shape, off)) = cached {
                                    if cached_shape == shape_id && (off as u16) < field_count {
                                        let idx = slab_start as usize + off;
                                        break 'ic self.arena_slab[idx].clone();
                                    }
                                }
                                let shape =
                                    &self.program.record_shapes[shape_id as usize];
                                let target_name = match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) => s.as_str(),
                                    _ => return Err(VmError::TypeMismatch(
                                        "expected FieldName const".into())),
                                };
                                let mut found: Option<usize> = None;
                                for (i, fn_const_idx) in shape.iter().enumerate() {
                                    if let Const::FieldName(s) =
                                        &self.program.constants[*fn_const_idx as usize]
                                    {
                                        if s == target_name { found = Some(i); break; }
                                    }
                                }
                                let off = found.ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{target_name}` on arena record")))?;
                                self.field_ics[fid][sid] = Some((shape_id, off));
                                self.arena_slab[slab_start as usize + off].clone()
                            };
                            self.stack.push(value);
                        }
                        other => return Err(VmError::TypeMismatch(
                            format!("GetField on non-record: {other:?}"))),
                    }
                }
                Op::GetElem(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Tuple(items) => {
                            let v = items.into_iter().nth(i as usize)
                                .ok_or_else(|| VmError::TypeMismatch(format!("tuple index {i} out of range")))?;
                            self.stack.push(v);
                        }
                        // #464 tuple codegen: positional read out of a
                        // frame-local tuple. The arena slot is an O(1)
                        // index, mirroring the heap path's nth().
                        Value::StackTuple { slab_start, arity } => {
                            if i >= arity {
                                return Err(VmError::TypeMismatch(
                                    format!("tuple index {i} out of range")));
                            }
                            let v = self.stack_record_arena[slab_start as usize + i as usize].clone();
                            self.stack.push(v);
                        }
                        // #463 slice 2a: positional read out of an
                        // arena tuple — same O(1) index pattern as
                        // StackTuple but through `arena_slab`.
                        Value::ArenaTuple { slab_start, arity } => {
                            if i >= arity {
                                return Err(VmError::TypeMismatch(
                                    format!("tuple index {i} out of range")));
                            }
                            let v = self.arena_slab[slab_start as usize + i as usize].clone();
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetElem on non-tuple: {other:?}"))),
                    }
                }
                Op::TestVariant(i) => {
                    let name = match &self.program.constants[i as usize] {
                        Const::VariantName(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected VariantName const".into())),
                    };
                    let v = self.pop()?;
                    match &v {
                        Value::Variant { name: vname, .. } => {
                            self.stack.push(Value::Bool(vname == &name));
                        }
                        // For tag-only enums of primitive type (e.g. ParseError = Empty | NotNumber)
                        // the value is currently a Variant too, since constructors emit MakeVariant.
                        other => return Err(VmError::TypeMismatch(format!("TestVariant on non-variant: {other:?}"))),
                    }
                }
                Op::GetVariant(_i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Variant { args, .. } => {
                            self.stack.push(Value::Tuple(args));
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetVariant on non-variant: {other:?}"))),
                    }
                }
                Op::GetVariantArg(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::Variant { mut args, .. } => {
                            if (i as usize) >= args.len() {
                                return Err(VmError::TypeMismatch("variant arg index oob".into()));
                            }
                            self.stack.push(args.swap_remove(i as usize));
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetVariantArg on non-variant: {other:?}"))),
                    }
                }
                Op::GetListLen => {
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => self.stack.push(Value::Int(items.len() as i64)),
                        other => return Err(VmError::TypeMismatch(format!("GetListLen on non-list: {other:?}"))),
                    }
                }
                Op::GetListElem(i) => {
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => {
                            let v = items.into_iter().nth(i as usize)
                                .ok_or_else(|| VmError::TypeMismatch("list index oob".into()))?;
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetListElem on non-list: {other:?}"))),
                    }
                }
                Op::GetListElemDyn => {
                    // Stack: [list, idx]
                    let idx = match self.pop()? {
                        Value::Int(n) => n as usize,
                        other => return Err(VmError::TypeMismatch(format!("GetListElemDyn idx: {other:?}"))),
                    };
                    let v = self.pop()?;
                    match v {
                        Value::List(items) => {
                            let v = items.into_iter().nth(idx)
                                .ok_or_else(|| VmError::TypeMismatch("list index oob".into()))?;
                            self.stack.push(v);
                        }
                        other => return Err(VmError::TypeMismatch(format!("GetListElemDyn on non-list: {other:?}"))),
                    }
                }
                Op::ListAppend => {
                    let value = self.pop()?;
                    let list = self.pop()?;
                    match list {
                        Value::List(mut items) => {
                            items.push_back(value);
                            self.stack.push(Value::List(items));
                        }
                        other => return Err(VmError::TypeMismatch(format!("ListAppend on non-list: {other:?}"))),
                    }
                }
                Op::Jump(off) => {
                    let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                    self.frames[frame_idx].pc = new_pc;
                }
                Op::JumpIf(off) => {
                    let v = self.pop()?;
                    if v.as_bool() {
                        let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                        self.frames[frame_idx].pc = new_pc;
                    }
                }
                Op::JumpIfNot(off) => {
                    let v = self.pop()?;
                    if !v.as_bool() {
                        let new_pc = (self.frames[frame_idx].pc as i32 + off) as usize;
                        self.frames[frame_idx].pc = new_pc;
                    }
                }
                Op::MakeClosure { fn_id, capture_count } => {
                    let n = capture_count as usize;
                    let mut captures: Vec<Value> = (0..n).map(|_| Value::Unit).collect();
                    for i in (0..n).rev() { captures[i] = self.pop()?; }
                    // Look up the canonical body hash so the resulting
                    // `Value::Closure` carries it for equality (#222).
                    let body_hash = self.program.functions[fn_id as usize].body_hash;
                    self.stack.push(Value::Closure { fn_id, body_hash, captures });
                }
                Op::CallClosure { arity, node_id_idx } => {
                    let arity = arity as usize;
                    // Args sit on the value stack at [args_base..]; the
                    // closure sits just below them at args_base - 1. Take
                    // the closure out (leaving a Unit placeholder), then
                    // write its captures and pop the args directly into
                    // the callee's locals — no per-call args Vec and no
                    // `captures.extend(args)` realloc (#464). The combined
                    // [captures, args] view the tracer wants is exactly
                    // the contiguous locals slice we just filled.
                    let args_base = self.stack.len() - arity;
                    let closure = std::mem::replace(&mut self.stack[args_base - 1], Value::Unit);
                    let (fn_id, captures) = match closure {
                        Value::Closure { fn_id, captures, .. } => (fn_id, captures),
                        other => return Err(VmError::TypeMismatch(format!("CallClosure on non-closure: {other:?}"))),
                    };
                    let fid = fn_id as usize;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    let cap_n = captures.len();
                    let locals_start = self.locals_storage.len();
                    let locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    self.locals_storage.resize(locals_start + locals_len, Value::Unit);
                    for (i, v) in captures.into_iter().enumerate() {
                        self.locals_storage[locals_start + i] = v;
                    }
                    // Move the args off the value stack into the locals
                    // following the captures (popping leaves the args off
                    // the stack; the closure's Unit placeholder is then
                    // the top, so truncate it away).
                    for i in (0..arity).rev() {
                        self.locals_storage[locals_start + cap_n + i] = self.pop()?;
                    }
                    self.stack.truncate(args_base - 1);
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.locals_storage[locals_start..locals_start + cap_n + arity]);
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals_start, locals_len,
                        stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                        // Op::CallClosure intentionally doesn't memoize
                        // for v1 (#229) — closures over captures need a
                        // hashing strategy that includes the captures.
                        // Direct Op::Call is the v1 surface.
                        memo_key: None,
                        stack_record_arena_start: self.stack_record_arena.len(),
                        stack_record_budget_remaining: STACK_RECORD_BUDGET_SLOTS,
                    })?;
                }
                Op::SortByKey { node_id_idx: _ } => {
                    // #338: pop (xs, f). For each x in xs, invoke
                    // f(x) to derive a sortable key. Stable-sort the
                    // (key, value) pairs by key. Return the values
                    // in sorted order. Keys must be Int / Float /
                    // Str; mixed-type pairs and other types compare
                    // as equal (preserving original order — stable
                    // sort).
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("SortByKey requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("SortByKey requires a closure, got: {f:?}")));
                    }
                    let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(items.len());
                    for item in items {
                        let key = self.invoke_closure_1(f.clone(), item.clone())?;
                        keyed.push((key, item));
                    }
                    keyed.sort_by(|(ka, _), (kb, _)| compare_sort_keys(ka, kb));
                    let sorted: VecDeque<Value> = keyed.into_iter().map(|(_, v)| v).collect();
                    self.stack.push(Value::List(sorted.into()));
                }
                Op::ParallelMap { node_id_idx: _ } => {
                    // #305 slice 1: pop (xs, f) and apply f to each
                    // element across OS threads.
                    //
                    // #305 slice 2: each worker now asks the parent
                    // handler for a thread-safe per-worker handler via
                    // `EffectHandler::spawn_for_worker`. Handlers that
                    // opt in (e.g. `DefaultHandler`) yield a fresh
                    // instance sharing the budget pool; handlers that
                    // don't fall back to the slice-1 behavior of
                    // `DenyAllEffects` in the worker.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ParallelMap requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ParallelMap requires a closure, got: {f:?}")));
                    }
                    // Pre-build one handler per worker on the main
                    // thread so the worker just owns its handler with
                    // no shared borrowing. The actual worker count is
                    // capped by `LEX_PAR_MAX_CONCURRENCY` (resolved
                    // inside par_map_run); cap ≤ items.len() so we
                    // never over-allocate handlers.
                    let n_workers = par_max_concurrency().max(1).min(items.len().max(1));
                    let mut worker_handlers: Vec<Box<dyn EffectHandler + Send>> =
                        Vec::with_capacity(n_workers);
                    for _ in 0..n_workers {
                        worker_handlers.push(
                            self.handler
                                .spawn_for_worker()
                                .unwrap_or_else(|| Box::new(DenyAllEffects)),
                        );
                    }
                    let results = par_map_run(self.program, f, items.into_iter().collect(), worker_handlers, self.step_limit)?;
                    self.stack.push(Value::List(results.into()));
                }
                Op::ListMap { node_id_idx: _ } => {
                    // #464: native map. Owns `xs` (no per-iteration
                    // clone of the input or accumulator that the old
                    // inlined `LoadLocal`-based loop incurred) and
                    // builds the output with one pre-sized allocation.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListMap requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListMap requires a closure, got: {f:?}")));
                    }
                    let mut out: VecDeque<Value> = VecDeque::with_capacity(items.len());
                    for item in items {
                        out.push_back(self.invoke_closure_1(f.clone(), item)?);
                    }
                    self.stack.push(Value::List(out.into()));
                }
                Op::ListFilter { node_id_idx: _ } => {
                    // #464: native filter. Pred is applied to a clone
                    // of each element; the original element is kept on
                    // a true result.
                    let f = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListFilter requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListFilter requires a closure, got: {f:?}")));
                    }
                    let mut out: VecDeque<Value> = VecDeque::new();
                    for item in items {
                        let keep = self.invoke_closure_1(f.clone(), item.clone())?;
                        if keep.as_bool() {
                            out.push_back(item);
                        }
                    }
                    self.stack.push(Value::List(out.into()));
                }
                Op::ListFold { node_id_idx: _ } => {
                    // #464: native left-fold. `acc` is threaded by
                    // value; each element is moved into the combiner.
                    let f = self.pop()?;
                    let init = self.pop()?;
                    let xs = self.pop()?;
                    let items = match xs {
                        Value::List(v) => v,
                        other => return Err(VmError::TypeMismatch(
                            format!("ListFold requires a List, got: {other:?}"))),
                    };
                    if !matches!(f, Value::Closure { .. }) {
                        return Err(VmError::TypeMismatch(
                            format!("ListFold requires a closure, got: {f:?}")));
                    }
                    let mut acc = init;
                    for item in items {
                        acc = self.invoke_closure_2(f.clone(), acc, item)?;
                    }
                    self.stack.push(acc);
                }
                Op::Call { fn_id, arity, node_id_idx } => {
                    let arity = arity as usize;
                    let fid = fn_id as usize;
                    // Args sit on the value stack at [args_base..]. We
                    // read them in place for the refinement / memo /
                    // trace checks and only move them into the locals
                    // slot-allocator at the very end — avoiding a
                    // per-call args Vec (#464 call-overhead). The stack
                    // naturally holds the args until consumed, so the
                    // only early-exit cleanup is truncating them off on
                    // a memo hit; a refinement error aborts the VM.
                    let args_base = self.stack.len() - arity;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    // Refinement runtime check (#209 slice 3). Each
                    // param's `Option<Refinement>` is evaluated against
                    // the actual arg before the frame is pushed. The
                    // tracer sees the call enter; failure surfaces as
                    // `VmError::RefinementFailed` *before* the body
                    // starts, which means an erroring trace shows the
                    // call as enter+exit_err with the verdict reason
                    // (same shape as `gate.verdict`).
                    //
                    // Iterate by reference — the loop body reads only
                    // through `r` (borrowed from `self.program`) and the
                    // arg slots on the stack; we don't mutate `self`, so
                    // the borrows are disjoint.
                    let refinements = &self.program.functions[fid].refinements;
                    for (i, refinement) in refinements.iter().enumerate() {
                        if let Some(r) = refinement {
                            let arg = self.stack[args_base + i].clone();
                            match eval_refinement(&r.predicate, &r.binding, &arg) {
                                Ok(true) => { /* satisfied, continue */ }
                                Ok(false) => {
                                    return Err(VmError::RefinementFailed {
                                        fn_name: self.program.functions[fid].name.clone(),
                                        param_index: i,
                                        binding: r.binding.clone(),
                                        reason: format!(
                                            "predicate failed for {} = {arg:?}",
                                            r.binding),
                                    });
                                }
                                Err(reason) => {
                                    return Err(VmError::RefinementFailed {
                                        fn_name: self.program.functions[fid].name.clone(),
                                        param_index: i,
                                        binding: r.binding.clone(),
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    // Pure-fn memoization (#229): if the callee declares
                    // no effects, hash the args and consult the cache.
                    // On hit, push the cached value, emit synthetic
                    // enter+exit trace events (so the trace still shows
                    // the call), and skip the frame push entirely.
                    //
                    // Adaptive gate (#229 adaptive): only hash if this
                    // function still has memoization enabled. A pure
                    // function whose args never repeat pays the hash for
                    // nothing; after a warmup window with zero hits we
                    // disable it and its calls take the plain path below.
                    let memo_key: Option<(u32, [u8; 16])> =
                        if self.program.functions[fid].effects.is_empty()
                            && self.memo_fn_state[fid].enabled
                            // #621: skip memo if any arg contains a request-scoped
                            // arena handle. The memo cache outlives the request arena,
                            // so hashing such a handle would dangle.
                            && !self.stack[args_base..].iter().any(|v| v.contains_arena_record())
                            // #764: never pay more than a bounded hash per call.
                            && !memo_args_exceed(&self.stack[args_base..], MEMO_MAX_ARG_BYTES)
                        {
                            Some((fn_id, hash_call_args(&self.stack[args_base..])))
                        } else {
                            if self.program.functions[fid].effects.is_empty() {
                                self.pure_memo_skips += 1;
                            }
                            None
                        };
                    if let Some(key) = memo_key {
                        self.memo_fn_state[fid].calls += 1;
                        if let Some(cached) = self.pure_memo.get(&key).cloned() {
                            self.memo_fn_state[fid].hits += 1;
                            self.pure_memo_hits += 1;
                            self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                            self.tracer.exit_ok(&cached);
                            self.stack.truncate(args_base);
                            self.stack.push(cached);
                            continue;
                        }
                        self.pure_memo_misses += 1;
                        // Disable on a cold function: warmup elapsed with
                        // no hit. Always safe — the callee is pure, so the
                        // plain path recomputes the identical result.
                        let st = &mut self.memo_fn_state[fid];
                        if st.calls >= MEMO_WARMUP_CALLS && st.hits == 0 {
                            st.enabled = false;
                        }
                    }
                    // #465 JIT tier hook. Consulted after refinements +
                    // memo. The hook contract (see `crate::jit_hook`)
                    // requires the dispatcher to emit the synthetic
                    // tracer events itself — we do that on hit, then
                    // truncate the args off the stack and push the
                    // result, mirroring the memo-hit path above.
                    //
                    // Take/restore around the call so the hook can
                    // borrow `&self.stack` for its args slice while
                    // we hold `&mut hook`. Cheaper than cloning the
                    // args; the take/put is two pointer writes.
                    if let Some(mut hook) = self.jit_hook.take() {
                        let step_ptr = &mut self.steps as *mut u64;
                        let limit = self.step_limit;
                        let hook_result = hook.try_call(fn_id, &self.stack[args_base..], step_ptr, limit);
                        self.jit_hook = Some(hook);
                        match hook_result? {
                            Some(result) => {
                                self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                                self.tracer.exit_ok(&result);
                                // Memoize the result if memo is enabled
                                // for this fn — same semantics as a
                                // regular call's Return path.
                                if let Some(key) = memo_key {
                                    self.pure_memo.insert(key, result.clone());
                                }
                                self.stack.truncate(args_base);
                                self.stack.push(result);
                                continue;
                            }
                            None => { /* hook declined; fall through */ }
                        }
                    }
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                    let locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    let locals_start = self.locals_storage.len();
                    self.locals_storage.resize(locals_start + locals_len, Value::Unit);
                    // Move the args off the stack into the callee's
                    // locals (popping leaves the stack at `args_base`).
                    for i in (0..arity).rev() {
                        self.locals_storage[locals_start + i] = self.pop()?;
                    }
                    self.push_frame(Frame {
                        fn_id, pc: 0, locals_start, locals_len,
                        stack_base: self.stack.len(),
                        trace_kind: FrameKind::Call(node_id),
                        memo_key,
                        stack_record_arena_start: self.stack_record_arena.len(),
                        stack_record_budget_remaining: STACK_RECORD_BUDGET_SLOTS,
                    })?;
                }
                Op::TailCall { fn_id, arity, node_id_idx } => {
                    let arity = arity as usize;
                    let fid = fn_id as usize;
                    // Args sit on the value stack at [args_base..]. Read
                    // them in place for the refinement / trace checks and
                    // move them into the reused frame's locals at the end
                    // — no per-call args Vec (#464). Tail calls have no
                    // memoization, so the consumers are refinement, trace,
                    // then the locals move. The args live on `self.stack`
                    // while locals live on `self.locals_storage`, so the
                    // `truncate(old_locals_start)` below (which releases
                    // the *old* frame's locals) doesn't touch them.
                    let args_base = self.stack.len() - arity;
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    let budget_cost = call_budget_cost(&self.program.functions[fid]);
                    if budget_cost > 0 {
                        self.handler.note_call_budget(budget_cost)
                            .map_err(VmError::Effect)?;
                    }
                    // Refinement runtime check on tail calls too
                    // (#209 slice 3). Same shape as Op::Call.
                    let refinements = &self.program.functions[fid].refinements;
                    for (i, refinement) in refinements.iter().enumerate() {
                        if let Some(r) = refinement {
                            let arg = self.stack[args_base + i].clone();
                            match eval_refinement(&r.predicate, &r.binding, &arg) {
                                Ok(true) => {}
                                Ok(false) => return Err(VmError::RefinementFailed {
                                    fn_name: self.program.functions[fid].name.clone(),
                                    param_index: i,
                                    binding: r.binding.clone(),
                                    reason: format!(
                                        "predicate failed for {} = {arg:?}",
                                        r.binding),
                                }),
                                Err(reason) => return Err(VmError::RefinementFailed {
                                    fn_name: self.program.functions[fid].name.clone(),
                                    param_index: i,
                                    binding: r.binding.clone(),
                                    reason,
                                }),
                            }
                        }
                    }
                    // #465 JIT tier hook for tail calls. A tail-called
                    // function's result IS the current frame's result,
                    // so on a hook hit we collapse the current frame:
                    // truncate state back to the frame's entry, emit
                    // the synthetic enter+exit_ok trace events that a
                    // normal tail-into-return would have produced, then
                    // bubble the result up the same way Op::Return
                    // does.
                    if let Some(mut hook) = self.jit_hook.take() {
                        let step_ptr = &mut self.steps as *mut u64;
                        let limit = self.step_limit;
                        let hook_result = hook.try_call(fn_id, &self.stack[args_base..], step_ptr, limit);
                        self.jit_hook = Some(hook);
                        if let Some(result) = hook_result? {
                            self.tracer.exit_call_tail();
                            self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                            self.tracer.exit_ok(&result);
                            let frame = self.frames.pop().unwrap();
                            self.stack.truncate(frame.stack_base);
                            self.locals_storage.truncate(frame.locals_start);
                            self.stack_record_arena.truncate(frame.stack_record_arena_start);
                            // Tail calls don't carry a memo_key (the
                            // existing arm doesn't memoize them), so
                            // skip the memo store the Return path does.
                            if self.frames.len() <= base_depth {
                                return Ok(result);
                            }
                            self.stack.push(result);
                            continue;
                        }
                    }
                    // A tail call closes the current call's trace frame and
                    // opens a new one in its place — preserves the caller's
                    // tree depth in the trace.
                    self.tracer.exit_call_tail();
                    self.tracer.enter_call(&node_id, &self.program.functions[fid].name, &self.stack[args_base..]);
                    // Reuse the current frame's locals_start position:
                    // truncate to release old locals then extend for the
                    // new function (#389 slice 3, same as Op::Return but
                    // without popping the frame).
                    let old_locals_start = self.frames.last().unwrap().locals_start;
                    self.locals_storage.truncate(old_locals_start);
                    let new_locals_len = self.program.functions[fid].locals_count
                        .max(self.program.functions[fid].arity) as usize;
                    self.locals_storage.resize(old_locals_start + new_locals_len, Value::Unit);
                    // Move the args off the value stack into the callee's
                    // locals (popping leaves the stack at `args_base`).
                    for i in (0..arity).rev() {
                        self.locals_storage[old_locals_start + i] = self.pop()?;
                    }
                    // #464 step 2: a tail-called function gets a fresh
                    // stack-record arena view. Release any records the
                    // pre-tail-call code allocated (they can't be live
                    // — the args have already been popped off the
                    // value stack) and refill the budget for the
                    // callee.
                    let arena_start = self.frames.last().unwrap().stack_record_arena_start;
                    self.stack_record_arena.truncate(arena_start);
                    let frame = self.frames.last_mut().unwrap();
                    frame.fn_id = fn_id;
                    frame.pc = 0;
                    frame.locals_len = new_locals_len;
                    frame.trace_kind = FrameKind::Call(node_id);
                    frame.stack_record_budget_remaining = STACK_RECORD_BUDGET_SLOTS;
                }
                Op::EffectCall { kind_idx, op_idx, arity, node_id_idx } => {
                    let mut args: Vec<Value> = (0..arity).map(|_| Value::Unit).collect();
                    for i in (0..arity as usize).rev() { args[i] = self.pop()?; }
                    // An arg built inside a live request scope (e.g. a tuple
                    // literal from a closure called while handling a
                    // `net.serve` request) may be a `Value::ArenaTuple` /
                    // `ArenaRecord` handle rather than its heap form. Every
                    // pure builtin and effect handler downstream (map.from_list,
                    // the tracer, …) pattern-matches on the heap Tuple/Record/
                    // List shapes and has no arena access to resolve a handle
                    // itself — so this is the single choke point where every
                    // effect/builtin call's args must be materialized before
                    // they cross that boundary (see `materialize_arena_handles`
                    // and docs/design/arena-plumbing.md). Gated on the arena
                    // slab being non-empty: if no request scope has allocated
                    // anything yet, no arg can possibly hold an arena handle,
                    // so this is a free no-op outside `net.serve` handlers.
                    if !self.arena_slab.is_empty() {
                        for a in args.iter_mut() {
                            let v = std::mem::replace(a, Value::Unit);
                            *a = self.materialize_arena_handles(v);
                        }
                    }
                    let kind = match &self.program.constants[kind_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected Str const for effect kind".into())),
                    };
                    let op_name = match &self.program.constants[op_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(VmError::TypeMismatch("expected Str const for effect op".into())),
                    };
                    let node_id = const_str(&self.program.constants, node_id_idx);
                    self.tracer.enter_effect(&node_id, &kind, &op_name, &args);
                    let result = match self.tracer.override_effect(&node_id) {
                        Some(v) => Ok(v),
                        // VM-level intercept for `parser.run` (#221).
                        // Routed inline rather than through the handler
                        // because the parser interpreter needs reentrant
                        // VM access to invoke `Value::Closure` values
                        // from `Map` / `AndThen` nodes.
                        None if (kind.as_str(), op_name.as_str()) == ("parser", "run")
                            => self.run_parser_op(args),
                        // VM-level intercept for `conc.*` (#381). The actor
                        // handler closure must run on the calling VM so it can
                        // dispatch arbitrary effects through the same handler
                        // chain (e.g. sql queries inside an actor).
                        None if kind.as_str() == "conc"
                            => self.run_conc_op(op_name.as_str(), args),
                        None => self.handler.dispatch(&kind, &op_name, args),
                    };
                    match result {
                        Ok(v) => {
                            self.tracer.exit_ok(&v);
                            self.stack.push(v);
                        }
                        Err(e) => {
                            self.tracer.exit_err(&e);
                            return Err(VmError::Effect(e));
                        }
                    }
                }
                Op::Return => {
                    let v = self.pop()?;
                    let frame = self.frames.pop().unwrap();
                    // Trim any extra stuff that the function pushed but didn't pop.
                    self.stack.truncate(frame.stack_base);
                    // Release this frame's locals back to the arena (#389 slice 3).
                    // LIFO frame ordering guarantees this frame's slots are at the top.
                    self.locals_storage.truncate(frame.locals_start);
                    // #464 step 2: release this frame's stack-record
                    // slab. LIFO frame discipline guarantees its
                    // records sit at the top of the arena. The
                    // returned value `v` is escape-proven not to be
                    // one of them — the compiler only emits
                    // AllocStackRecord at sites that don't reach
                    // `Return`.
                    self.stack_record_arena.truncate(frame.stack_record_arena_start);
                    if matches!(frame.trace_kind, FrameKind::Call(_)) {
                        self.tracer.exit_ok(&v);
                    }
                    // Pure-fn memoization (#229): if this frame was a
                    // memoizable call that missed the cache, write the
                    // computed return value back so the next call with
                    // the same args returns it without re-executing.
                    if let Some(key) = frame.memo_key {
                        self.pure_memo.insert(key, v.clone());
                    }
                    // Exit when we've returned past the depth this
                    // `run_to` was entered at — supports reentrancy
                    // (a nested `invoke` returns into its caller, not
                    // out of the outermost VM run, #221).
                    if self.frames.len() <= base_depth {
                        return Ok(v);
                    }
                    self.stack.push(v);
                }
                Op::Panic(i) => {
                    let msg = match &self.program.constants[i as usize] {
                        Const::Str(s) => s.clone(),
                        _ => "panic".into(),
                    };
                    return Err(VmError::Panic(msg));
                }
                // Arithmetic
                Op::IntAdd => self.bin_int(|a, b| Value::Int(a + b))?,
                Op::IntSub => self.bin_int(|a, b| Value::Int(a - b))?,
                Op::IntMul => self.bin_int(|a, b| Value::Int(a * b))?,
                Op::IntDiv => self.bin_int_divmod(false)?,
                Op::IntMod => self.bin_int_divmod(true)?,
                Op::IntNeg => {
                    let a = self.pop()?.as_int();
                    self.stack.push(Value::Int(-a));
                }
                Op::IntEq => self.bin_int(|a, b| Value::Bool(a == b))?,
                Op::IntLt => self.bin_int(|a, b| Value::Bool(a < b))?,
                Op::IntLe => self.bin_int(|a, b| Value::Bool(a <= b))?,
                Op::FloatAdd => self.bin_float(|a, b| Value::Float(a + b))?,
                Op::FloatSub => self.bin_float(|a, b| Value::Float(a - b))?,
                Op::FloatMul => self.bin_float(|a, b| Value::Float(a * b))?,
                Op::FloatDiv => self.bin_float(|a, b| Value::Float(a / b))?,
                Op::FloatNeg => {
                    let a = self.pop()?.as_float();
                    self.stack.push(Value::Float(-a));
                }
                Op::FloatEq => self.bin_float(|a, b| Value::Bool(a == b))?,
                Op::FloatLt => self.bin_float(|a, b| Value::Bool(a < b))?,
                Op::FloatLe => self.bin_float(|a, b| Value::Bool(a <= b))?,
                Op::NumAdd => {
                    // #308: `+` is overloaded — Str+Str concatenates,
                    // numerics add. Other arithmetic ops (-, *, /, %)
                    // still reject Str at the type-checker layer.
                    let b = self.pop()?;
                    let a = self.pop()?;
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => self.stack.push(Value::Int(x + y)),
                        (Value::Float(x), Value::Float(y)) => self.stack.push(Value::Float(x + y)),
                        (Value::Str(x), Value::Str(y)) => {
                            // SmolStr is immutable; concatenate via a temporary String.
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(&x);
                            s.push_str(&y);
                            self.stack.push(Value::Str(s.into()));
                        }
                        (a, b) => return Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
                    }
                }
                Op::NumSub => self.bin_num(|a, b| Value::Int(a - b), |a, b| Value::Float(a - b))?,
                Op::NumMul => self.bin_num(|a, b| Value::Int(a * b), |a, b| Value::Float(a * b))?,
                Op::NumDiv => self.num_divmod(false)?,
                Op::NumMod => self.num_divmod(true)?,
                Op::NumNeg => {
                    let v = self.pop()?;
                    match v {
                        Value::Int(n) => self.stack.push(Value::Int(-n)),
                        Value::Float(f) => self.stack.push(Value::Float(-f)),
                        other => return Err(VmError::TypeMismatch(format!("NumNeg on {other:?}"))),
                    }
                }
                Op::NumEq => self.bin_eq()?,
                Op::NumLt => self.bin_ord(|a, b| Value::Bool(a < b), |a, b| Value::Bool(a < b), |a, b| Value::Bool(a < b))?,
                Op::NumLe => self.bin_ord(|a, b| Value::Bool(a <= b), |a, b| Value::Bool(a <= b), |a, b| Value::Bool(a <= b))?,
                Op::BoolAnd => {
                    let b = self.pop()?.as_bool();
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(a && b));
                }
                Op::BoolOr => {
                    let b = self.pop()?.as_bool();
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(a || b));
                }
                Op::BoolNot => {
                    let a = self.pop()?.as_bool();
                    self.stack.push(Value::Bool(!a));
                }
                Op::StrConcat => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let s = format!("{}{}", a.as_str(), b.as_str());
                    self.stack.push(Value::Str(s.into()));
                }
                Op::StrLen => {
                    let v = self.pop()?;
                    self.stack.push(Value::Int(v.as_str().len() as i64));
                }
                Op::StrEq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Value::Bool(a.as_str() == b.as_str()));
                }
                Op::BytesLen => {
                    let v = self.pop()?;
                    match v {
                        Value::Bytes(b) => self.stack.push(Value::Int(b.len() as i64)),
                        other => return Err(VmError::TypeMismatch(format!("BytesLen on {other:?}"))),
                    }
                }
                Op::BytesEq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let eq = match (a, b) {
                        (Value::Bytes(x), Value::Bytes(y)) => x == y,
                        _ => return Err(VmError::TypeMismatch("BytesEq operands".into())),
                    };
                    self.stack.push(Value::Bool(eq));
                }

                // Superinstructions (#461).
                Op::LoadLocalAddIntConst { local_idx, imm_const_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + local_idx as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        c => return Err(VmError::TypeMismatch(
                            format!("LoadLocalAddIntConst expected Int const, got {c:?}"))),
                    };
                    self.stack.push(Value::Int(a + b));
                    // Override the default `pc + 1`: skip past the
                    // two inert primitive ops (the original
                    // PushConst + IntAdd) that the peephole pass
                    // left in place for body-hash stability.
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalAddLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a + b));
                    // Override the default `pc + 1`: skip past the
                    // two inert primitive ops (the original
                    // LoadLocal(rhs_idx) + IntAdd) that the peephole
                    // pass left in place for body-hash stability.
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalSubLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a - b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalMulLocal { lhs_idx, rhs_idx } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + lhs_idx as usize].as_int();
                    let b = self.locals_storage[base + rhs_idx as usize].as_int();
                    self.stack.push(Value::Int(a * b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetField { local_idx, name_idx, site_idx } => {
                    // #461 slice 9: fused `LoadLocal + GetField`. Reads
                    // the field directly out of the local record by
                    // reference and pushes it, advancing pc by 2 (one
                    // tombstone — the original GetField). Avoids the
                    // unfused pair's whole-record clone onto the value
                    // stack: the dominant heap-record churn on the
                    // `response_build` profile (`r.total` field reads).
                    let base = self.frames[frame_idx].locals_start;
                    let v = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetField")?;
                    self.stack.push(v);
                    self.frames[frame_idx].pc = pc + 2;
                }
                Op::LoadLocalGetFieldAdd { local_idx, name_idx, site_idx } => {
                    // #461 slice 7: fused `LoadLocal + GetField + IntAdd`.
                    // Pop the prior stack top (the accumulator), read the
                    // field by reference (shared IC via
                    // `read_local_record_field`), push the sum, advance
                    // pc by 3 (skip the GetField and IntAdd tombstones).
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldAdd")?.as_int();
                    self.stack.push(Value::Int(acc + b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetFieldSub { local_idx, name_idx, site_idx } => {
                    // #461 slice 8: `LoadLocal + GetField + IntSub`. The
                    // `acc - r.field` idiom. IntSub computes
                    // deeper-minus-top; the field was on top in the
                    // unfused form, so the result is `acc - field`.
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldSub")?.as_int();
                    self.stack.push(Value::Int(acc - b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalGetFieldMul { local_idx, name_idx, site_idx } => {
                    // #461 slice 8: `LoadLocal + GetField + IntMul`. The
                    // `acc * r.field` idiom (mul is commutative, so
                    // operand order doesn't matter).
                    let acc = self.pop()?.as_int();
                    let base = self.frames[frame_idx].locals_start;
                    let b = self.read_local_record_field(
                        base, local_idx, fn_id, name_idx, site_idx, "LoadLocalGetFieldMul")?.as_int();
                    self.stack.push(Value::Int(acc * b));
                    self.frames[frame_idx].pc = pc + 3;
                }
                Op::LoadLocalEqIntConstJumpIfNot { local_idx, imm_const_idx, jump_offset } => {
                    // First jump-aware fusion (#461 slice 5). The
                    // JumpIfNot's offset is relative to its own
                    // pc + 1 = (pc + 3) + 1 = pc + 4, so the branch
                    // target is `pc + 4 + jump_offset`. Fall-through
                    // (equal → JumpIfNot doesn't jump) is `pc + 4`
                    // (skip past the 3 tombstones — PushConst +
                    // IntEq + JumpIfNot).
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + local_idx as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        _ => return Err(VmError::TypeMismatch(
                            "LoadLocalEqIntConstJumpIfNot expects Const::Int".into())),
                    };
                    let next_pc = if a == b {
                        pc + 4
                    } else {
                        ((pc as i32 + 4) + jump_offset) as usize
                    };
                    self.frames[frame_idx].pc = next_pc;
                }
                Op::LoadLocalStoreEqIntConstJumpIfNot { src, dst, imm_const_idx, jump_offset } => {
                    // Slice 6: absorbs LoadLocal + StoreLocal + slice-5 op.
                    // 6-slot window total (this op + 5 tombstones); fall-
                    // through is `pc + 6`, branch target is `pc + 6 +
                    // jump_offset` (the original JumpIfNot was at slot
                    // pc+5, with offset relative to its own pc+1 = pc+6).
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + src as usize].as_int();
                    // Mirror the original `StoreLocal(dst)` — later
                    // arm tests in the same `match` expect to find
                    // the scrutinee at `locals[dst]`.
                    self.locals_storage[base + dst as usize] = Value::Int(a);
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        _ => return Err(VmError::TypeMismatch(
                            "LoadLocalStoreEqIntConstJumpIfNot expects Const::Int".into())),
                    };
                    let next_pc = if a == b {
                        pc + 6
                    } else {
                        ((pc as i32 + 6) + jump_offset) as usize
                    };
                    self.frames[frame_idx].pc = next_pc;
                }
                Op::LoadLocalAddIntConstStoreLocal { src, imm_const_idx, dest } => {
                    let base = self.frames[frame_idx].locals_start;
                    let a = self.locals_storage[base + src as usize].as_int();
                    let b = match &self.program.constants[imm_const_idx as usize] {
                        Const::Int(n) => *n,
                        c => return Err(VmError::TypeMismatch(
                            format!("LoadLocalAddIntConstStoreLocal expected Int const, got {c:?}"))),
                    };
                    self.locals_storage[base + dest as usize] = Value::Int(a + b);
                    // Skip past the 3 inert primitive ops we
                    // absorbed (original PushConst + IntAdd +
                    // StoreLocal).
                    self.frames[frame_idx].pc = pc + 4;
                }
            }
        }
    }

    pub(super) fn pop(&mut self) -> Result<Value, VmError> {
        self.stack.pop().ok_or(VmError::StackUnderflow)
    }
    pub(super) fn peek(&self) -> Result<&Value, VmError> {
        self.stack.last().ok_or(VmError::StackUnderflow)
    }

    /// IC-cached field read of `locals[local_idx]`, shared by the
    /// field-read fusions: slice 9's `LoadLocalGetField` and slice
    /// 7/8's `LoadLocalGetField{Add,Sub,Mul}`. Uses the same
    /// `(fn_id, site_idx)` inline-cache slot as the unfused
    /// `Op::GetField`, so the paths stay cache-consistent.
    /// `op_name` only appears in the non-record error message.
    ///
    /// Reads the record **by reference** and clones out only the
    /// selected field — it does *not* clone the whole record. The
    /// unfused `[LoadLocal, GetField]` pair clones the entire record
    /// (`Box<IndexMap>` for a heap record) onto the value stack just
    /// to read one field and drop the rest; on the `response_build`
    /// profile that whole-record clone+drop of the returned `Response`
    /// dominated the malloc traffic. Borrowing in place removes it.
    ///
    /// Borrow discipline: the inline-cache slot can't be written while
    /// the record (a borrow of `self.locals_storage`) is live, so the
    /// match yields `(value, install)` and the `field_ics` write
    /// happens after the borrow ends.
    ///
    /// `#[inline(always)]`: hot dispatch path, called from four tight
    /// `run_to` arms; leaving it out-of-line showed up as a standalone
    /// call frame on the profile.
    #[inline(always)]
    pub(super) fn read_local_record_field(
        &mut self,
        base: usize,
        local_idx: u16,
        fn_id: u32,
        name_idx: u32,
        site_idx: u32,
        op_name: &str,
    ) -> Result<Value, VmError> {
        let fid = fn_id as usize;
        let sid = site_idx as usize;
        if self.field_ics[fid].is_empty() {
            let n = self.program.functions[fid].field_ic_sites as usize;
            self.field_ics[fid] = vec![None; n];
        }
        let cached = self.field_ics[fid][sid];
        let li = base + local_idx as usize;

        let (value, install): (Value, Option<(u32, usize)>) =
            match &self.locals_storage[li] {
                Value::Record { fields: r, shape_id } => {
                    let shape_id = *shape_id;
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    let hit = if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id {
                            if shape_id != crate::value::NO_SHAPE_ID {
                                r.get_index(off).map(|(_, val)| val.clone())
                            } else if let Some((k, val)) = r.get_index(off) {
                                match &self.program.constants[name_idx as usize] {
                                    Const::FieldName(s) if s == k => Some(val.clone()),
                                    _ => None,
                                }
                            } else { None }
                        } else { None }
                    } else { None };
                    match hit {
                        Some(v) => (v, None),
                        None => {
                            let name = match &self.program.constants[name_idx as usize] {
                                Const::FieldName(s) => s.as_str(),
                                _ => return Err(VmError::TypeMismatch(
                                    "expected FieldName const".into())),
                            };
                            let (off, _, val) = r.get_full(name)
                                .ok_or_else(|| VmError::TypeMismatch(
                                    format!("missing field `{name}`")))?;
                            (val.clone(), Some((shape_id, off)))
                        }
                    }
                }
                &Value::StackRecord { shape_id, slab_start, field_count } => {
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id && (off as u16) < field_count {
                            let idx = slab_start as usize + off;
                            (self.stack_record_arena[idx].clone(), None)
                        } else {
                            let off = self.resolve_stack_field(shape_id, name_idx)?;
                            (self.stack_record_arena[slab_start as usize + off].clone(),
                             Some((shape_id, off)))
                        }
                    } else {
                        let off = self.resolve_stack_field(shape_id, name_idx)?;
                        (self.stack_record_arena[slab_start as usize + off].clone(),
                         Some((shape_id, off)))
                    }
                }
                // #463 slice 2a: superinstruction read out of an
                // arena-allocated record held in a local. Same shape
                // resolution as the stack-record arm (records share
                // the same `record_shapes` table regardless of
                // allocation site); only the slab indexed differs.
                &Value::ArenaRecord { shape_id, slab_start, field_count } => {
                    if ic_stats_enabled() {
                        record_ic_hit(fn_id, site_idx, shape_id);
                    }
                    if let Some((cached_shape, off)) = cached {
                        if cached_shape == shape_id && (off as u16) < field_count {
                            let idx = slab_start as usize + off;
                            (self.arena_slab[idx].clone(), None)
                        } else {
                            let off = self.resolve_stack_field(shape_id, name_idx)?;
                            (self.arena_slab[slab_start as usize + off].clone(),
                             Some((shape_id, off)))
                        }
                    } else {
                        let off = self.resolve_stack_field(shape_id, name_idx)?;
                        (self.arena_slab[slab_start as usize + off].clone(),
                         Some((shape_id, off)))
                    }
                }
                other => return Err(VmError::TypeMismatch(
                    format!("{op_name} on non-record: {other:?}"))),
            };
        if let Some(entry) = install {
            self.field_ics[fid][sid] = Some(entry);
        }
        Ok(value)
    }

    /// Resolve a field offset within a stack-record shape by name
    /// (the slow path when the inline cache misses). Factored out so
    /// `read_local_record_field` doesn't hold the `locals_storage`
    /// borrow across the `record_shapes` / `constants` walk.
    #[inline]
    pub(super) fn resolve_stack_field(&self, shape_id: u32, name_idx: u32) -> Result<usize, VmError> {
        let shape = &self.program.record_shapes[shape_id as usize];
        let target_name = match &self.program.constants[name_idx as usize] {
            Const::FieldName(s) => s.as_str(),
            _ => return Err(VmError::TypeMismatch("expected FieldName const".into())),
        };
        for (i, fn_const_idx) in shape.iter().enumerate() {
            if let Const::FieldName(s) = &self.program.constants[*fn_const_idx as usize] {
                if s == target_name { return Ok(i); }
            }
        }
        Err(VmError::TypeMismatch(
            format!("missing field `{target_name}` on stack record")))
    }

    pub(super) fn bin_int(&mut self, f: impl Fn(i64, i64) -> Value) -> Result<(), VmError> {
        let b = self.pop()?.as_int();
        let a = self.pop()?.as_int();
        self.stack.push(f(a, b));
        Ok(())
    }
    /// Guarded integer `/` (`is_mod == false`) or `%` (`is_mod == true`)
    /// for `Op::IntDiv` / `Op::IntMod` (#696). A zero divisor raises
    /// `VmError::DivByZero` instead of panicking the host. `wrapping_*`
    /// also tames the only other panicking input, `i64::MIN / -1` (and
    /// `i64::MIN % -1`), whose true result overflows `i64`: division
    /// wraps to `i64::MIN`, modulo to `0`.
    pub(super) fn bin_int_divmod(&mut self, is_mod: bool) -> Result<(), VmError> {
        let b = self.pop()?.as_int();
        let a = self.pop()?.as_int();
        if b == 0 {
            return Err(VmError::DivByZero { op: if is_mod { "modulo" } else { "division" } });
        }
        let v = if is_mod { a.wrapping_rem(b) } else { a.wrapping_div(b) };
        self.stack.push(Value::Int(v));
        Ok(())
    }
    /// Guarded `/` / `%` for the overloaded `Op::NumDiv` / `Op::NumMod`,
    /// which accept either both-`Int` or both-`Float` operands (#696).
    /// Integers route through the same zero/overflow guards as
    /// `bin_int_divmod`; floats keep IEEE-754 semantics (inf/NaN, no
    /// trap). Mirrors the type checker, which only admits these two
    /// operand shapes for `%` (int) and `/` (int or float).
    pub(super) fn num_divmod(&mut self, is_mod: bool) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if y == 0 {
                    return Err(VmError::DivByZero { op: if is_mod { "modulo" } else { "division" } });
                }
                let v = if is_mod { x.wrapping_rem(y) } else { x.wrapping_div(y) };
                self.stack.push(Value::Int(v));
                Ok(())
            }
            (Value::Float(x), Value::Float(y)) => {
                self.stack.push(Value::Float(if is_mod { x % y } else { x / y }));
                Ok(())
            }
            (a, b) => Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
        }
    }
    pub(super) fn bin_float(&mut self, f: impl Fn(f64, f64) -> Value) -> Result<(), VmError> {
        let b = self.pop()?.as_float();
        let a = self.pop()?.as_float();
        self.stack.push(f(a, b));
        Ok(())
    }
    pub(super) fn bin_num(
        &mut self,
        i: impl Fn(i64, i64) -> Value,
        f: impl Fn(f64, f64) -> Value,
    ) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => { self.stack.push(i(x, y)); Ok(()) }
            (Value::Float(x), Value::Float(y)) => { self.stack.push(f(x, y)); Ok(()) }
            (a, b) => Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
        }
    }

    /// Like `bin_num` but also handles `Str` operands via lexicographic order.
    /// Used by `NumLt` / `NumLe` because the type checker admits `Str < Str`
    /// and `>` / `>=` compile as swap+NumLt / swap+NumLe (#332).
    pub(super) fn bin_ord(
        &mut self,
        i: impl Fn(i64, i64) -> Value,
        f: impl Fn(f64, f64) -> Value,
        s: impl Fn(&str, &str) -> Value,
    ) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => { self.stack.push(i(x, y)); Ok(()) }
            (Value::Float(x), Value::Float(y)) => { self.stack.push(f(x, y)); Ok(()) }
            (Value::Str(x), Value::Str(y)) => { self.stack.push(s(&x, &y)); Ok(()) }
            (a, b) => Err(VmError::TypeMismatch(format!("Num op: {a:?} {b:?}"))),
        }
    }
    pub(super) fn bin_eq(&mut self) -> Result<(), VmError> {
        let b = self.pop()?;
        let a = self.pop()?;
        self.stack.push(Value::Bool(a == b));
        Ok(())
    }
}
