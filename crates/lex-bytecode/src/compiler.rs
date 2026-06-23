//! M4 compiler: canonical AST → bytecode.

use crate::op::*;
use crate::program::*;
use indexmap::IndexMap;
use lex_ast as a;

#[derive(Default)]
struct ConstPool {
    pool: Vec<Const>,
    fields: IndexMap<String, u32>,
    variants: IndexMap<String, u32>,
    node_ids: IndexMap<String, u32>,
    ints: IndexMap<i64, u32>,
    bools: IndexMap<u8, u32>,
    strs: IndexMap<String, u32>,
    /// Interned record field-name shapes (#461). Deduplicated by content
    /// so a record literal with the same field-name layout reuses the
    /// same `shape_idx` across the whole program — keeps the side-table
    /// small even when the same struct is constructed in many places.
    record_shapes: Vec<Vec<u32>>,
    record_shape_dedup: IndexMap<Vec<u32>, u32>,
}

impl ConstPool {
    fn field(&mut self, name: &str) -> u32 {
        if let Some(i) = self.fields.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::FieldName(name.into()));
        self.fields.insert(name.into(), i);
        i
    }
    fn variant(&mut self, name: &str) -> u32 {
        if let Some(i) = self.variants.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::VariantName(name.into()));
        self.variants.insert(name.into(), i);
        i
    }
    fn node_id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.node_ids.get(name) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::NodeId(name.into()));
        self.node_ids.insert(name.into(), i);
        i
    }
    fn int(&mut self, n: i64) -> u32 {
        if let Some(i) = self.ints.get(&n) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Int(n));
        self.ints.insert(n, i);
        i
    }
    fn bool(&mut self, b: bool) -> u32 {
        let key = b as u8;
        if let Some(i) = self.bools.get(&key) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Bool(b));
        self.bools.insert(key, i);
        i
    }
    fn str(&mut self, s: &str) -> u32 {
        if let Some(i) = self.strs.get(s) { return *i; }
        let i = self.pool.len() as u32;
        self.pool.push(Const::Str(s.into()));
        self.strs.insert(s.into(), i);
        i
    }
    fn float(&mut self, f: f64) -> u32 {
        // Floats: not deduped (NaN issues).
        let i = self.pool.len() as u32;
        self.pool.push(Const::Float(f));
        i
    }
    fn unit(&mut self) -> u32 {
        let i = self.pool.len() as u32;
        self.pool.push(Const::Unit);
        i
    }

    /// Intern a record field-name shape (#461). Returns the index into
    /// `record_shapes`; identical shapes (same field-name-index vec)
    /// always return the same index.
    fn record_shape(&mut self, idxs: Vec<u32>) -> u32 {
        if let Some(i) = self.record_shape_dedup.get(&idxs) {
            return *i;
        }
        let i = self.record_shapes.len() as u32;
        self.record_shape_dedup.insert(idxs.clone(), i);
        self.record_shapes.push(idxs);
        i
    }
}

pub fn compile_program(stages: &[a::Stage]) -> Program {
    let mut p = Program {
        constants: Vec::new(),
        functions: Vec::new(),
        function_names: IndexMap::new(),
        module_aliases: IndexMap::new(),
        entry: None,
        record_shapes: Vec::new(),
    };

    // Collect imports as alias → module-name. The module name is the part
    // after `std.` (so `import "std.io" as io` ⇒ alias `io` → module `io`).
    for s in stages {
        if let a::Stage::Import(i) = s {
            let module = i.reference.strip_prefix("std.").unwrap_or(&i.reference).to_string();
            p.module_aliases.insert(i.alias.clone(), module);
        }
    }

    for s in stages {
        if let a::Stage::FnDecl(fd) = s {
            let idx = p.functions.len() as u32;
            p.function_names.insert(fd.name.clone(), idx);
            p.functions.push(Function {
                name: fd.name.clone(),
                arity: fd.params.len() as u16,
                locals_count: 0,
                code: Vec::new(),
                effects: fd.effects.iter().map(|e| DeclaredEffect {
                    kind: e.name.clone(),
                    arg: e.arg.as_ref().map(|a| match a {
                        a::EffectArg::Str { value } => EffectArg::Str(value.clone()),
                        a::EffectArg::Int { value } => EffectArg::Int(*value),
                        a::EffectArg::Ident { value } => EffectArg::Ident(value.clone()),
                    }),
                }).collect(),
                // Filled in at the end of the compile pass, once `code`
                // and `locals_count` are final. See #222.
                body_hash: crate::program::ZERO_BODY_HASH,
                // Per-param refinement predicates for runtime check
                // (#209 slice 3). Lifted directly from each param's
                // `TypeExpr::Refined` if present; `None` otherwise.
                refinements: fd.params.iter().map(|p| match &p.ty {
                    a::TypeExpr::Refined { binding, predicate, .. } =>
                        Some(crate::program::Refinement {
                            binding: binding.clone(),
                            predicate: (**predicate).clone(),
                        }),
                    _ => None,
                }).collect(),
                // Filled in below once the FnCompiler counts emit sites.
                field_ic_sites: 0,
            });
        }
    }

    let mut pool = ConstPool::default();
    let function_names = p.function_names.clone();
    let module_aliases = p.module_aliases.clone();
    let mut pending_lambdas: Vec<PendingLambda> = Vec::new();
    // #461 slice 7: collect `type Foo = { ... }` aliases so
    // `record_field_types` can resolve a parameter's named record
    // type to its field layout. Without this, `r :: R` where
    // `type R = { x :: Int, y :: Int }` falls through to Unknown
    // and the typed-Add lowering misses on `r.x + r.y`.
    let mut type_aliases: IndexMap<String, a::TypeExpr> = IndexMap::new();
    for s in stages {
        if let a::Stage::TypeDecl(td) = s {
            // Parameterized type aliases (`type Box[T] = ...`) are
            // out of scope for this slice — without monomorphization
            // we can't know what T resolves to. Skip them.
            if td.params.is_empty() {
                type_aliases.insert(td.name.clone(), td.definition.clone());
            }
        }
    }

    for s in stages {
        if let a::Stage::FnDecl(_) = s {
            // Build a NodeId map for *this* stage so the compiler can stamp
            // each Call/EffectCall opcode with the originating AST node.
            let id_map = lex_ast::expr_ids(s);
            let fd = match s { a::Stage::FnDecl(fd) => fd, _ => unreachable!() };
            let mut fc = FnCompiler {
                code: Vec::new(),
                locals: IndexMap::new(),
                next_local: 0,
                peak_local: 0,
                local_types: IndexMap::new(),
                local_record_field_types: IndexMap::new(),
                field_get_sites: 0,
                pool: &mut pool,
                function_names: &function_names,
                module_aliases: &module_aliases,
                id_map: &id_map,
                pending_lambdas: &mut pending_lambdas,
                next_fn_id: &mut p.functions,
            };
            for param in &fd.params {
                let i = fc.next_local;
                fc.locals.insert(param.name.clone(), i);
                fc.local_types.insert(param.name.clone(), classify_type_expr(&param.ty));
                // #461 slice 7: inline-record parameter (`r ::
                // { x :: Int, y :: Int }`) — populate the per-local
                // field-type map so `r.x + r.y` classifies as
                // Int+Int → IntAdd, which slice 7 then fuses.
                if let Some(ftypes) = record_field_types(&param.ty, &type_aliases) {
                    fc.local_record_field_types.insert(param.name.clone(), ftypes);
                }
                fc.next_local += 1;
                fc.peak_local = fc.next_local;
            }
            fc.compile_expr(&fd.body, true);
            fc.code.push(Op::Return);
            let code = std::mem::take(&mut fc.code);
            let peak = fc.peak_local;
            let field_sites = fc.field_get_sites as u16;
            drop(fc);
            let idx = function_names[&fd.name];
            p.functions[idx as usize].code = code;
            p.functions[idx as usize].field_ic_sites = field_sites;
            p.functions[idx as usize].locals_count = peak;
        }
    }

    // Compile pending lambdas in FIFO order. Each lambda may emit further
    // lambdas; loop until the queue drains.
    while let Some(pl) = pending_lambdas.pop() {
        let id_map = std::collections::HashMap::new();
        let mut fc = FnCompiler {
            code: Vec::new(),
            locals: IndexMap::new(),
            next_local: 0,
            peak_local: 0,
            local_types: IndexMap::new(),
            local_record_field_types: IndexMap::new(),
            field_get_sites: 0,
            pool: &mut pool,
            function_names: &function_names,
            module_aliases: &module_aliases,
            id_map: &id_map,
            pending_lambdas: &mut pending_lambdas,
            next_fn_id: &mut p.functions,
        };
        for name in &pl.capture_names {
            let i = fc.next_local;
            fc.locals.insert(name.clone(), i);
            // Captures' static types aren't known at this layer
            // — the closure's environment carries them dynamically.
            // Conservative fallback; binop lowering stays correct
            // because Unknown classifies through to NumAdd.
            fc.local_types.insert(name.clone(), NumTy::Unknown);
            fc.next_local += 1;
            fc.peak_local = fc.next_local;
        }
        for p in &pl.params {
            let i = fc.next_local;
            fc.locals.insert(p.name.clone(), i);
            fc.local_types.insert(p.name.clone(), classify_type_expr(&p.ty));
            fc.next_local += 1;
            fc.peak_local = fc.next_local;
        }
        fc.compile_expr(&pl.body, true);
        fc.code.push(Op::Return);
        let code = std::mem::take(&mut fc.code);
        let peak = fc.peak_local;
        let field_sites = fc.field_get_sites as u16;
        drop(fc);
        p.functions[pl.fn_id as usize].code = code;
        p.functions[pl.fn_id as usize].field_ic_sites = field_sites;
        p.functions[pl.fn_id as usize].locals_count = peak;
    }

    // #464 step 2: escape-analysis-driven lowering. Rewrites
    // `MakeRecord` at non-escaping sites to `AllocStackRecord`, which
    // the VM allocates in the frame's stack-record arena instead of
    // on the heap. Runs on raw bytecode (before the peephole passes)
    // so the escape analysis — which itself walks raw bytecode — sees
    // exactly the program it was designed for.
    //
    // The peephole passes that follow do not match on MakeRecord /
    // AllocStackRecord, so swapping one for the other doesn't disturb
    // any pattern. `compute_body_hash` lowers AllocStackRecord back
    // to the legacy MakeRecord form (#222), so closure identity is
    // invariant under this lowering.
    //
    // Escape hatch: `LEX_NO_STACK_RECORDS=1` skips the lowering
    // entirely (#464 step 3). The flag exists so the bench can A/B
    // the same source under matched VM/peephole conditions; in
    // production code the pass always runs.
    if std::env::var_os("LEX_NO_STACK_RECORDS").is_none() {
        let escape_index = crate::escape::build_escape_index(&p.functions);
        for f in p.functions.iter_mut() {
            apply_escape_lowering(&mut f.code, &f.name, &escape_index);
        }
    }

    // #463 slice 2b-i: arena-eligibility lowering. Runs **after**
    // `apply_escape_lowering` and targets the remaining `MakeRecord`
    // / `MakeTuple` sites — those the stack pass left alone because
    // they cross the frame boundary, but the request-scope analysis
    // proves they stay inside the active `EffectHandler` arena
    // scope. The two passes form a three-tier allocation hierarchy:
    //
    //   frame-local        → AllocStackRecord  (#464, cheapest)
    //   request-local      → AllocArenaRecord  (#463, this slice)
    //   escapes request    → MakeRecord        (heap, status quo)
    //
    // Order matters: a site that fits the stack tier should land
    // there (cheapest), so the stack pass runs first. The arena
    // pass's match doesn't fire on AllocStackRecord, so already-
    // stack-lowered sites stay stack-lowered. Sites that escape the
    // frame and the request both pass through unchanged.
    //
    // Escape hatch: `LEX_NO_ARENA_RECORDS=1` skips the lowering,
    // mirroring `LEX_NO_STACK_RECORDS`. The slice-2b-i bench uses
    // this to A/B identical source under matched VM conditions.
    //
    // `body_hash` invariance: `compute_body_hash` decodes
    // `AllocArenaRecord` / `AllocArenaTuple` back to their legacy
    // `MakeRecord` / `MakeTuple` form, so closure identity (#222) is
    // bit-identical across this and the stack lowering.
    if std::env::var_os("LEX_NO_ARENA_RECORDS").is_none() {
        let arena_index = crate::arena::build_arena_index(&p.functions);
        for f in p.functions.iter_mut() {
            apply_arena_lowering(&mut f.code, &f.name, &arena_index);
        }
    }

    // Peephole pass (#461 superinstructions). Rewrites fusable opcode
    // patterns into single dispatch steps. Runs before `body_hash`
    // computation, but `compute_body_hash` decomposes each fused op
    // back to its primitive form on hash — so closure identity (#222)
    // is invariant under this pass and the order doesn't matter.
    //
    // Slices run sequentially: slice 2 looks for slice-1 output
    // followed by a StoreLocal, so it must follow slice 1. Slice 3
    // (LoadLocal + LoadLocal + IntAdd) is disjoint from both — its
    // second slot is LoadLocal, not PushConst — so it can run in
    // either order. Run it last to keep the slice 1/2 contract
    // (slice 2 expects to see slice-1 output) untouched. Slice 4 is
    // slice 3 for IntSub / IntMul (same pattern, different terminator);
    // disjoint from every prior slice because the terminator op
    // disambiguates, so order between slice 3 and slice 4 is free.
    for f in p.functions.iter_mut() {
        apply_peephole(&mut f.code, &pool.pool);
        apply_peephole_slice2(&mut f.code);
        apply_peephole_slice3(&mut f.code);
        apply_peephole_slice4(&mut f.code);
        // Slice 5 — jump-aware fusion of the loop-condition idiom
        // (LoadLocal + LoadLocal/PushConst + IntLt + JumpIfNot).
        // Runs after slices 3/4 because their 3-slot windows
        // overlap slice 5's 4-slot window at position 0 and 1; if
        // slice 3 fired first and consumed `LoadLocal + LoadLocal +
        // IntAdd`, the `IntLt + JumpIfNot` that follows would not
        // be a fusion candidate. Since slice 3's terminator is
        // `IntAdd` and slice 5's is `IntLt`, the two don't compete
        // on the same site — order between them is technically free
        // but conventionally slice N runs after slice N-1.
        apply_peephole_slice5(&mut f.code, &pool.pool);
        // Slice 6 — absorb the match-scrutinee dance
        // (`LoadLocal + StoreLocal` immediately preceding a slice-5
        // fused op that reads the just-stored local). Must run after
        // slice 5 since it matches on slice 5's output.
        apply_peephole_slice6(&mut f.code);
        // Slice 7/8 — fuse `LoadLocal + GetField + IntAdd|IntSub|IntMul`,
        // the accumulator-with-field-read idiom. Disjoint from every
        // earlier slice (only this one matches a GetField at slot 1),
        // so order is independent — placed near the end for chronology.
        apply_peephole_slice7(&mut f.code);
        // Slice 9 — fuse the *remaining* bare `LoadLocal + GetField`
        // pairs (those slice 7/8 didn't consume): chain-head field
        // reads (`r.x` in `r.x + r.y`), standalone `r.field` reads
        // (`r.total`), and field reads feeding non-add/sub/mul ops.
        // MUST run after slice 7/8 — otherwise it would greedily eat
        // the `LoadLocal + GetField` prefix of an `acc OP r.field`
        // triple and prevent the 3-op fusion. Slice 7/8's tombstone
        // GetFields are preceded by their fused op (not a bare
        // LoadLocal), so slice 9 never matches them.
        apply_peephole_slice9(&mut f.code);
    }

    // Final pass: stamp every function with its content hash now that
    // every body is finalized (#222). Trampolines installed via
    // `install_trampoline` already have it; recomputing is cheap and
    // makes the invariant easier to read at this top level.
    for f in p.functions.iter_mut() {
        if f.body_hash == crate::program::ZERO_BODY_HASH {
            f.body_hash = crate::program::compute_body_hash(
                f.arity, f.locals_count, &f.code, &pool.record_shapes);
        }
    }

    p.constants = pool.pool;
    p.record_shapes = pool.record_shapes;
    p
}

/// Peephole pass: rewrite fusable opcode patterns into superinstructions
/// (#461). Each fused op claims its own slot in the code stream; the
/// trailing primitive ops it absorbs stay in place as inert
/// "tombstones" — the dispatch loop overrides its default `pc += 1`
/// to step past them. Leaving the tombstones in place keeps
/// `code.len()` invariant and means we don't have to renumber jump
/// offsets.
///
/// Pattern (slice 1): `LoadLocal(i), PushConst(c), IntAdd` where
/// `constants[c]` is a `Const::Int`. Fused to
/// `LoadLocalAddIntConst { local_idx: i, imm_const_idx: c }`.
/// Safety: the second and third slots must not be reachable from
/// any Jump / JumpIf / JumpIfNot — otherwise a jump would land on a
/// tombstone instead of the live op the source intended. The
/// pre-pass below collects every jump target in the function and
/// skips fusion sites whose tombstones overlap one.
/// #464 step 2 — rewrite `MakeRecord` to `AllocStackRecord` at sites
/// the escape analysis (`crate::escape::build_escape_index`) proved
/// non-escaping. Each rewrite is a single-slot swap that preserves
/// pc, stack delta, and shape semantics — jump targets, the peephole
/// passes downstream, and the body-hash decoder all see the same
/// program shape they would have seen for the unlowered code.
///
/// Sites that escape are left as-is and still incur the
/// IndexMap-backed heap allocation. Step 3 of #464 carries the
/// bench acceptance bars (≥1.5× speedup on `response_build`); this
/// pass is the precondition.
fn apply_escape_lowering(
    code: &mut [Op],
    fn_name: &str,
    escape_index: &std::collections::HashMap<(String, u32), bool>,
) {
    for (pc, op) in code.iter_mut().enumerate() {
        // Look up this (fn, pc) in the escape index. Absent → analysis
        // didn't observe the site (defensive: leave on heap path).
        // Present and false → safe to stack-allocate. Each rewrite is a
        // single-slot swap preserving pc / stack delta, so jump
        // targets, downstream peephole passes, and the body-hash
        // decoder all see the same program shape.
        let key = (fn_name.to_string(), pc as u32);
        if !matches!(escape_index.get(&key), Some(false)) {
            continue;
        }
        match *op {
            Op::MakeRecord { shape_idx, field_count } => {
                *op = Op::AllocStackRecord { shape_idx, field_count };
            }
            // #464 tuple codegen: same single-slot swap as records.
            Op::MakeTuple(arity) => {
                *op = Op::AllocStackTuple { arity };
            }
            _ => {}
        }
    }
}

/// #463 slice 2b-i — rewrite `MakeRecord` / `MakeTuple` to the arena
/// variants at sites the request-scope analysis
/// (`crate::arena::build_arena_index`) proved do not escape the
/// active `EffectHandler` arena scope.
///
/// Only fires on **remaining** `MakeRecord` / `MakeTuple` sites — the
/// stack pass (`apply_escape_lowering`) runs first and converts the
/// non-frame-escaping cheaper-tier sites. Sites that escape both the
/// frame *and* the request stay as `MakeRecord` / `MakeTuple` (heap),
/// untouched.
///
/// Each rewrite is the same single-slot swap as the stack lowering:
/// pc / stack delta / shape semantics preserved, jump targets and
/// downstream peephole passes see the same program shape, and
/// `compute_body_hash` (#222) decodes both arena ops back to their
/// legacy `MakeRecord` / `MakeTuple` form so closure identity is
/// invariant.
fn apply_arena_lowering(
    code: &mut [Op],
    fn_name: &str,
    arena_index: &std::collections::HashMap<(String, u32), bool>,
) {
    for (pc, op) in code.iter_mut().enumerate() {
        // arena_index value: true = arena-eligible. Absent or false
        // → leave on heap (defensive default; absent means the
        // analysis didn't observe the site).
        let key = (fn_name.to_string(), pc as u32);
        if !matches!(arena_index.get(&key), Some(true)) {
            continue;
        }
        match *op {
            Op::MakeRecord { shape_idx, field_count } => {
                *op = Op::AllocArenaRecord { shape_idx, field_count };
            }
            Op::MakeTuple(arity) => {
                *op = Op::AllocArenaTuple { arity };
            }
            _ => {}
        }
    }
}

fn apply_peephole(code: &mut [Op], constants: &[Const]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(local_idx), Op::PushConst(imm_const_idx), Op::IntAdd)
            = (code[k], code[k + 1], code[k + 2])
        {
            let imm_is_int = matches!(
                constants.get(imm_const_idx as usize),
                Some(Const::Int(_))
            );
            // Tombstones at k+1 and k+2 must not be jump targets;
            // k itself can be a target (it stays a live op — the
            // fused form executes the same semantics in one step).
            let safe = imm_is_int
                && !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2));
            if safe {
                code[k] = Op::LoadLocalAddIntConst { local_idx, imm_const_idx };
                k += 3;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 2: fuse `[LoadLocalAddIntConst, _, _, StoreLocal(dest)]`
/// into `LoadLocalAddIntConstStoreLocal { src, imm_const_idx, dest }`.
/// The two `_` slots are slice-1 tombstones (the original PushConst
/// and IntAdd) and stay in place as slice-2 tombstones too. The
/// dispatch loop advances pc by 4 past all three trailing slots
/// after executing the fused op.
fn apply_peephole_slice2(code: &mut [Op]) {
    if code.len() < 4 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 3 < n {
        if let (
            Op::LoadLocalAddIntConst { local_idx: src, imm_const_idx },
            _,
            _,
            Op::StoreLocal(dest),
        ) = (code[k], code[k + 1], code[k + 2], code[k + 3])
        {
            // Slice-1 contract: code[k+1] is the original
            // PushConst(imm_const_idx) and code[k+2] is the
            // original IntAdd. We don't re-verify those — slice 1
            // is the only producer of LoadLocalAddIntConst and
            // always leaves the contract intact.
            //
            // Safety: tombstones at k+1..k+3 must not be reachable
            // from any jump. k itself can be (it's still a live
            // op carrying the same semantics).
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2))
                && !jump_targets.contains(&(k + 3));
            if safe {
                code[k] = Op::LoadLocalAddIntConstStoreLocal {
                    src,
                    imm_const_idx,
                    dest,
                };
                k += 4;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 3: fuse `[LoadLocal(lhs), LoadLocal(rhs), IntAdd]` into
/// `LoadLocalAddLocal { lhs_idx, rhs_idx }`. The binary-op-on-two-
/// locals idiom: any `a + b` where both operands compile to a
/// `LoadLocal` (typed `Int`). Mirrors slice 1's shape exactly — the
/// trailing `LoadLocal` + `IntAdd` stay in place as inert tombstones
/// with cancelling stack deltas (+1, -1), so the verifier and
/// body-hash decoder both keep walking them as live.
///
/// Disjoint from slice 1: the second slot disambiguates (LoadLocal
/// vs PushConst), so a site can match at most one of the two. Runs
/// after slice 2 so we don't accidentally consume a `LoadLocal` slot
/// that slice 2 was about to fuse into a `*StoreLocal` superop (and
/// to keep slice 2's input contract — slice-1 output followed by
/// StoreLocal — untouched).
///
/// Safety: like slice 1, the trailing two slots must not be jump
/// targets. The first slot can be a target (it stays a live op).
fn apply_peephole_slice3(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(lhs_idx), Op::LoadLocal(rhs_idx), Op::IntAdd)
            = (code[k], code[k + 1], code[k + 2])
        {
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2));
            if safe {
                code[k] = Op::LoadLocalAddLocal { lhs_idx, rhs_idx };
                k += 3;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 4: slice 3 for `IntSub` and `IntMul`. Fuses
/// `[LoadLocal(lhs), LoadLocal(rhs), IntSub]` to
/// `LoadLocalSubLocal { lhs_idx, rhs_idx }` and the `IntMul` shape
/// to `LoadLocalMulLocal`. Same tombstone, jump-safety, and
/// body-hash story as slice 3 — the trailing two slots stay as
/// inert primitives with cancelling stack deltas.
///
/// Disjoint from every prior slice: slice 1/2 require a `PushConst`
/// at slot 2 (here it's `LoadLocal`), and slice 3's terminator is
/// `IntAdd` (here it's `IntSub` / `IntMul`). A given site matches at
/// most one slice.
fn apply_peephole_slice4(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(lhs_idx), Op::LoadLocal(rhs_idx), terminator)
            = (code[k], code[k + 1], code[k + 2])
        {
            let fused = match terminator {
                Op::IntSub => Some(Op::LoadLocalSubLocal { lhs_idx, rhs_idx }),
                Op::IntMul => Some(Op::LoadLocalMulLocal { lhs_idx, rhs_idx }),
                _ => None,
            };
            if let Some(fused_op) = fused {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = fused_op;
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 5: fuse the loop-condition idiom — 4-slot window
/// `[LoadLocal, LoadLocal|PushConst, IntLt, JumpIfNot(offset)]` —
/// into `LoadLocalLtLocalJumpIfNot` or `LoadLocalLtIntConstJumpIfNot`.
/// First jump-aware peephole in this codebase: the fused op carries
/// the JumpIfNot's offset and the VM dispatches directly to either
/// `pc + 4` (condition true, fall through past tombstones) or
/// `pc + 4 + offset` (condition false, original JumpIfNot target).
///
/// Safety conditions, on top of slice 1's "tombstones must not be
/// jump targets":
/// 1. Trailing 3 slots (k+1, k+2, k+3) must not be jump targets from
///    elsewhere — same as slice 1/3/4, just three of them.
/// 2. The slot at k+3 (JumpIfNot) is the one whose offset we copy
///    into the fused op. The offset is relative to the JumpIfNot's
///    `pc + 1` which equals `k + 4`, so the resolved target is
///    `k + 4 + offset` — that target must be safe to land on (it
///    already is, since JumpIfNot is operating as designed).
/// 3. The const-int branch checks the PushConst points at a
///    `Const::Int` — same as slice 1.
fn apply_peephole_slice5(code: &mut [Op], constants: &[Const]) {
    if code.len() < 4 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 3 < n {
        // Match the lhs slot — always a LoadLocal.
        let lhs_idx = match code[k] {
            Op::LoadLocal(i) => i,
            _ => { k += 1; continue; }
        };
        // Match the rhs slot — either LoadLocal or PushConst(Int).
        // The two flavors emit different fused ops.
        let fused = match (code[k + 1], code[k + 2], code[k + 3]) {
            (Op::PushConst(imm_const_idx), Op::IntEq, Op::JumpIfNot(jump_offset))
                if matches!(constants.get(imm_const_idx as usize), Some(Const::Int(_))) =>
                Some(Op::LoadLocalEqIntConstJumpIfNot {
                    local_idx: lhs_idx, imm_const_idx, jump_offset,
                }),
            _ => None,
        };
        if let Some(fused_op) = fused {
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2))
                && !jump_targets.contains(&(k + 3));
            if safe {
                code[k] = fused_op;
                k += 4;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 6: fuse the match-scrutinee dance preceding a slice-5
/// pattern-match arm test. 3-slot window
/// `[LoadLocal(src), StoreLocal(dst),
///   LoadLocalEqIntConstJumpIfNot { local_idx: dst, ... }]` —
/// where the slice-5 op's `local_idx` matches the StoreLocal's
/// destination — rewrites to
/// `LoadLocalStoreEqIntConstJumpIfNot { src, dst, ... }` at slot k.
/// The fused op carries `dst` so it can mirror the original
/// StoreLocal (later arm tests in the same match keep reading
/// `locals[dst]`).
///
/// Trailing tombstones: 5 slots (the original StoreLocal + the
/// slice-5 fused op itself + slice 5's 3 primitive tombstones).
/// VM dispatch skips them via `pc + 6`; verifier override pushes
/// `(pc + 6, ...)` and the branch target `(pc + 6 + jump_offset, ...)`
/// — the offset is identical to what slice 5 stored (still relative
/// to the original JumpIfNot's `pc + 1`, now at `k + 5 + 1 = k + 6`).
///
/// Safety: slots k+1..=k+5 must not be jump targets — same window
/// safety as the other slices. Slice 5 already verified k+3..=k+5
/// weren't jump targets when it fused; slice 6 only needs to re-check
/// k+1 (the StoreLocal) and k+2 (the slice-5 fused op).
fn apply_peephole_slice6(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (
            Op::LoadLocal(src),
            Op::StoreLocal(dst),
            Op::LoadLocalEqIntConstJumpIfNot { local_idx, imm_const_idx, jump_offset },
        ) = (code[k], code[k + 1], code[k + 2]) {
            // The slice-5 op must read the very local the StoreLocal
            // just wrote; if it reads some other local this isn't the
            // match-scrutinee idiom (could be a coincidental sequence).
            if local_idx == dst {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = Op::LoadLocalStoreEqIntConstJumpIfNot {
                        src, dst, imm_const_idx, jump_offset,
                    };
                    // Skip past this slice-6 window. The slice-5
                    // tombstones at k+3..=k+5 are already handled by
                    // slice 5's earlier rewrite; we don't need to
                    // touch them.
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 7/8: fuse `[LoadLocal(local_idx), GetField{name_idx,
/// site_idx}, IntAdd|IntSub|IntMul]` into the matching
/// `LoadLocalGetField{Add,Sub,Mul} { local_idx, name_idx, site_idx }`.
///
/// Fires on the `acc OP r.field` accumulator-with-field-read idiom —
/// the bytecode the compiler emits for `prev_expr OP record.field`
/// once `prev_expr` is on the stack. Common in handler-shaped code
/// like `r.x + r.y + r.z` (the LHS of each operator after the first
/// matches this pattern), `acc + items[i].weight` reductions, and
/// the `v.l - v.m` / `v.h * v.k` mixes the `response_build` profile
/// exercises.
///
/// Disjoint from every prior slice: slice 1 wants `PushConst` at
/// slot 1; slices 3-4 want `LoadLocal` at slot 1; slice 5 wants
/// a 4-slot window with `IntEq + JumpIfNot` terminator. Only this
/// slice matches a `GetField` at slot 1.
///
/// Order: must run after slice 4 (so the disjointness analysis
/// holds — slice 3/4 patterns with a trailing IntAdd / IntSub /
/// IntMul never carry a GetField at slot 1 and don't compete);
/// must run before / independent of slice 5/6, which don't match
/// any slot in this window.
///
/// Safety: trailing two slots (the original `GetField` and the
/// arithmetic op) must not be jump targets. The first slot can be.
fn apply_peephole_slice7(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(local_idx), Op::GetField { name_idx, site_idx })
            = (code[k], code[k + 1])
        {
            let fused = match code[k + 2] {
                Op::IntAdd => Some(Op::LoadLocalGetFieldAdd { local_idx, name_idx, site_idx }),
                Op::IntSub => Some(Op::LoadLocalGetFieldSub { local_idx, name_idx, site_idx }),
                Op::IntMul => Some(Op::LoadLocalGetFieldMul { local_idx, name_idx, site_idx }),
                _ => None,
            };
            if let Some(op) = fused {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = op;
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 9: fuse the bare `[LoadLocal(local_idx), GetField{name_idx,
/// site_idx}]` pair into `LoadLocalGetField { local_idx, name_idx,
/// site_idx }` — the plain `record.field` read, the most common
/// field-access shape.
///
/// The win is allocation, not just one fewer dispatch: the unfused
/// pair clones the entire record onto the value stack (a
/// `Box<IndexMap>` for a heap record) only to read one field; the
/// fused op reads the field out of the local by reference and clones
/// only that value. On `response_build` the whole-record clone of the
/// returned `Response` (`r.total`) was the dominant malloc source.
///
/// Order: MUST run after slice 7/8. Those fuse `[LoadLocal, GetField,
/// IntAdd|IntSub|IntMul]`; if slice 9 ran first it would consume the
/// `LoadLocal + GetField` prefix and block the 3-op fusion. After
/// slice 7/8, the only remaining `[LoadLocal, GetField]` pairs are
/// the ones they didn't want (chain heads, standalone reads, field
/// reads feeding other ops). Slice 7/8's tombstone GetFields sit
/// after their fused op, never after a bare `LoadLocal`, so slice 9
/// won't touch them.
///
/// Safety: the trailing slot (the original `GetField`) must not be a
/// jump target. The first slot can be.
fn apply_peephole_slice9(code: &mut [Op]) {
    if code.len() < 2 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 1 < n {
        if let (Op::LoadLocal(local_idx), Op::GetField { name_idx, site_idx })
            = (code[k], code[k + 1])
        {
            if !jump_targets.contains(&(k + 1)) {
                code[k] = Op::LoadLocalGetField { local_idx, name_idx, site_idx };
                k += 2;
                continue;
            }
        }
        k += 1;
    }
}

fn collect_jump_targets(code: &[Op]) -> std::collections::HashSet<usize> {
    let mut targets = std::collections::HashSet::new();
    for (pc, op) in code.iter().enumerate() {
        let off = match op {
            Op::Jump(off) | Op::JumpIf(off) | Op::JumpIfNot(off) => Some(*off),
            _ => None,
        };
        if let Some(off) = off {
            let target = (pc as i32 + 1 + off) as usize;
            targets.insert(target);
        }
    }
    targets
}

#[derive(Debug, Clone)]
struct PendingLambda {
    fn_id: u32,
    /// Names of captured outer-scope locals, in order.
    capture_names: Vec<String>,
    params: Vec<a::Param>,
    body: a::CExpr,
}

struct FnCompiler<'a> {
    code: Vec<Op>,
    locals: IndexMap<String, u16>,
    next_local: u16,
    /// Peak local usage seen during compilation (for VM frame sizing).
    peak_local: u16,
    /// Inferred numeric type of each local for typed numeric-op
    /// lowering (#461). Populated when binding function parameters
    /// (from their declared `TypeExpr::Named { name: "Int", .. }`
    /// or `"Float"`) and when binding `let name := value` where
    /// the RHS classifies statically. Used by `compile_binop` to
    /// emit `Op::IntAdd` / `Op::FloatAdd` instead of the
    /// polymorphic `Op::NumAdd` when both operands' types are
    /// statically known. Conservative: falls back to `NumTy::Unknown`
    /// (and the polymorphic op) whenever a type isn't locally
    /// derivable.
    ///
    /// Keyed by local *name* (parallel to `locals`) rather than by
    /// slot index so shadowed bindings are handled correctly via
    /// `IndexMap`'s insertion-order semantics.
    local_types: IndexMap<String, NumTy>,
    /// Per-local map of statically-known field types (#461 slice 7).
    /// Populated when a local is bound from a `RecordLit` whose
    /// fields all classify to non-`Unknown` `NumTy`s. Lets
    /// `classify_expr(FieldAccess { value: Var(name), field })`
    /// return a precise `NumTy` instead of falling back to
    /// `Unknown` — which in turn unlocks the typed-Add lowering
    /// (`+` over two Ints → `IntAdd`) on `r.field + r.field`
    /// chains, which slice 7 then fuses into
    /// `LoadLocalGetFieldAdd`.
    ///
    /// Only the literal-binding case is tracked here; annotated
    /// `let r :: R := ...` would require resolving the type alias
    /// `R` to its field-type map, which the compiler doesn't yet
    /// have. Future slice work.
    local_record_field_types: IndexMap<String, IndexMap<String, NumTy>>,
    /// Per-function counter for `Op::GetField` site indices (#462
    /// slice 1). Each `Op::GetField` emit allocates the next index
    /// here, giving every field-access site within this function a
    /// stable identifier independent of pc. The VM uses
    /// `(fn_id, site_idx)` as the inline-cache key, so the cache
    /// survives the future dispatch rewrite (#461) and a JIT (#465).
    field_get_sites: u32,
    pool: &'a mut ConstPool,
    function_names: &'a IndexMap<String, u32>,
    module_aliases: &'a IndexMap<String, String>,
    /// CExpr address → NodeId, populated per stage via `lex_ast::expr_ids`.
    id_map: &'a std::collections::HashMap<*const a::CExpr, lex_ast::NodeId>,
    /// Queue of lambdas discovered during compilation; each gets a fresh
    /// fn_id and is compiled in a later pass.
    pending_lambdas: &'a mut Vec<PendingLambda>,
    /// Mutable view of the function table — used to allocate fn_ids for
    /// freshly-discovered lambdas.
    next_fn_id: &'a mut Vec<Function>,
}

/// Lightweight numeric-type classification used by `compile_binop`
/// to decide whether to emit `IntAdd` / `FloatAdd` (specialized,
/// fast) or `NumAdd` (polymorphic, runtime-typed dispatch). #461
/// typed-lowering pass — conservative: anything not provably one
/// of these returns `Unknown` and falls back to the polymorphic op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumTy { Int, Float, Unknown }

/// #461 slice 7: extract a `field_name -> NumTy` map from a record
/// type expression. Resolves named types (`r :: R`) via
/// `type_aliases` and returns `None` if the type ultimately isn't
/// a record literal.
fn record_field_types(
    ty: &a::TypeExpr,
    type_aliases: &IndexMap<String, a::TypeExpr>,
) -> Option<IndexMap<String, NumTy>> {
    match ty {
        a::TypeExpr::Record { fields } => {
            let mut m = IndexMap::new();
            for f in fields {
                m.insert(f.name.clone(), classify_type_expr(&f.ty));
            }
            Some(m)
        }
        a::TypeExpr::Refined { base, .. } => record_field_types(base, type_aliases),
        a::TypeExpr::Named { name, args } if args.is_empty() => {
            // Resolve the alias and recurse. Cycle protection isn't
            // needed here — a cyclic type alias would have been
            // rejected by `lex-types::check_program` upstream.
            type_aliases.get(name).and_then(|t| record_field_types(t, type_aliases))
        }
        _ => None,
    }
}

fn classify_type_expr(ty: &a::TypeExpr) -> NumTy {
    match ty {
        a::TypeExpr::Named { name, args } if args.is_empty() => match name.as_str() {
            "Int" => NumTy::Int,
            "Float" => NumTy::Float,
            _ => NumTy::Unknown,
        },
        // `Refined { base, .. }` (#209) — classify by the base type;
        // the refinement predicate doesn't change the value's primitive shape.
        a::TypeExpr::Refined { base, .. } => classify_type_expr(base),
        _ => NumTy::Unknown,
    }
}

impl<'a> FnCompiler<'a> {
    fn alloc_local(&mut self, name: &str) -> u16 {
        let i = self.next_local;
        self.locals.insert(name.into(), i);
        self.next_local += 1;
        if self.next_local > self.peak_local { self.peak_local = self.next_local; }
        i
    }
    fn emit(&mut self, op: Op) { self.code.push(op); }

    fn compile_expr(&mut self, e: &a::CExpr, tail: bool) {
        match e {
            a::CExpr::Literal { value } => self.compile_lit(value),
            a::CExpr::Var { name } => {
                if let Some(slot) = self.locals.get(name) {
                    self.emit(Op::LoadLocal(*slot));
                } else if let Some(&fn_id) = self.function_names.get(name) {
                    // Function name used as a *value* (e.g. as a record-field
                    // initializer or fold-callback arg) — materialize it as a
                    // closure with no captures. The runtime already accepts
                    // `Value::Closure { fn_id, captures: vec![] }` and
                    // `CallClosure` dispatches it. (#169)
                    self.emit(Op::MakeClosure { fn_id, capture_count: 0 });
                } else {
                    // Should be caught at type-check time; the type checker
                    // walks every Var. If we land here it's a compiler bug,
                    // not a user typo.
                    panic!("unknown var in compiler: {name}");
                }
            }
            a::CExpr::Let { name, ty, value, body } => {
                // Classify the RHS for typed-op lowering (#461). Prefer
                // the declared annotation when present (cheap O(1)
                // lookup); fall back to classifying the value
                // expression structurally.
                let nty = match ty {
                    Some(t) => classify_type_expr(t),
                    None => self.classify_expr(value),
                };
                // #461 slice 7: when the RHS is a record literal,
                // remember the field types so `name.field` accesses
                // downstream can classify precisely. Without this,
                // `r.x + r.y` falls through to `NumAdd`, blocking
                // the slice-7 fusion.
                if let a::CExpr::RecordLit { fields } = value.as_ref() {
                    let mut ftypes = IndexMap::new();
                    for f in fields {
                        let fty = self.classify_expr(&f.value);
                        ftypes.insert(f.name.clone(), fty);
                    }
                    self.local_record_field_types.insert(name.clone(), ftypes);
                }
                self.compile_expr(value, false);
                let slot = self.alloc_local(name);
                self.local_types.insert(name.clone(), nty);
                self.emit(Op::StoreLocal(slot));
                self.compile_expr(body, tail);
            }
            a::CExpr::Block { statements, result } => {
                for s in statements {
                    self.compile_expr(s, false);
                    self.emit(Op::Pop);
                }
                self.compile_expr(result, tail);
            }
            a::CExpr::Call { callee, args } => self.compile_call(e, callee, args, tail),
            a::CExpr::Constructor { name, args } => {
                for a in args { self.compile_expr(a, false); }
                let name_idx = self.pool.variant(name);
                self.emit(Op::MakeVariant { name_idx, arity: args.len() as u16 });
            }
            a::CExpr::Match { scrutinee, arms } => self.compile_match(scrutinee, arms, tail),
            a::CExpr::RecordLit { fields } => {
                let mut idxs = Vec::with_capacity(fields.len());
                for f in fields {
                    self.compile_expr(&f.value, false);
                    idxs.push(self.pool.field(&f.name));
                }
                let field_count = idxs.len() as u16;
                let shape_idx = self.pool.record_shape(idxs);
                self.emit(Op::MakeRecord { shape_idx, field_count });
            }
            a::CExpr::TupleLit { items } => {
                for it in items { self.compile_expr(it, false); }
                self.emit(Op::MakeTuple(items.len() as u16));
            }
            a::CExpr::ListLit { items } => {
                for it in items { self.compile_expr(it, false); }
                self.emit(Op::MakeList(items.len() as u32));
            }
            a::CExpr::FieldAccess { value, field } => {
                self.compile_expr(value, false);
                let name_idx = self.pool.field(field);
                let site_idx = self.field_get_sites;
                self.field_get_sites += 1;
                self.emit(Op::GetField { name_idx, site_idx });
            }
            a::CExpr::BinOp { op, lhs, rhs } => self.compile_binop(op, lhs, rhs),
            a::CExpr::UnaryOp { op, expr } => {
                self.compile_expr(expr, false);
                match op.as_str() {
                    "-" => self.emit(Op::NumNeg),
                    "not" => self.emit(Op::BoolNot),
                    other => panic!("unknown unary: {other}"),
                }
            }
            a::CExpr::Lambda { params, body, .. } => self.compile_lambda(params, body),
            a::CExpr::Return { value } => {
                self.compile_expr(value, true);
                self.emit(Op::Return);
            }
        }
    }

    fn compile_lit(&mut self, l: &a::CLit) {
        let i = match l {
            a::CLit::Int { value } => self.pool.int(*value),
            a::CLit::Bool { value } => self.pool.bool(*value),
            a::CLit::Float { value } => {
                let f: f64 = value.parse().unwrap_or(0.0);
                self.pool.float(f)
            }
            a::CLit::Str { value } => self.pool.str(value),
            a::CLit::Bytes { value: _ } => {
                // Stub: M4 doesn't use bytes literals in §3.13 examples.
                let i = self.pool.pool.len() as u32;
                self.pool.pool.push(Const::Bytes(Vec::new()));
                i
            }
            a::CLit::Unit => self.pool.unit(),
        };
        self.emit(Op::PushConst(i));
    }

    fn compile_call(&mut self, call_expr: &a::CExpr, callee: &a::CExpr, args: &[a::CExpr], tail: bool) {
        let node_id = self
            .id_map
            .get(&(call_expr as *const a::CExpr))
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "n_?".into());
        let node_id_idx = self.pool.node_id(&node_id);

        // Module function call: `alias.op(args)` where `alias` is an imported
        // module ⇒ EffectCall, except for higher-order pure ops where we
        // emit inline bytecode using CallClosure (the closure-arg can't be
        // serialized through the effect handler).
        if let a::CExpr::FieldAccess { value, field } = callee {
            if let a::CExpr::Var { name } = value.as_ref() {
                if let Some(module) = self.module_aliases.get(name) {
                    if self.try_emit_higher_order(module, field, args, node_id_idx) {
                        let _ = tail;
                        return;
                    }
                    for a in args { self.compile_expr(a, false); }
                    let kind_idx = self.pool.str(module);
                    let op_idx = self.pool.str(field);
                    self.emit(Op::EffectCall {
                        kind_idx,
                        op_idx,
                        arity: args.len() as u16,
                        node_id_idx,
                    });
                    let _ = tail;
                    return;
                }
            }
        }
        match callee {
            a::CExpr::Var { name } if self.function_names.contains_key(name) => {
                for a in args { self.compile_expr(a, false); }
                let fn_id = self.function_names[name];
                if tail {
                    self.emit(Op::TailCall { fn_id, arity: args.len() as u16, node_id_idx });
                } else {
                    self.emit(Op::Call { fn_id, arity: args.len() as u16, node_id_idx });
                }
            }
            a::CExpr::Var { name } if self.locals.contains_key(name) => {
                // First-class function value bound to a local. Push the
                // closure, then args, then CallClosure.
                let slot = self.locals[name];
                self.emit(Op::LoadLocal(slot));
                for a in args { self.compile_expr(a, false); }
                self.emit(Op::CallClosure { arity: args.len() as u16, node_id_idx });
            }
            // Lambda directly applied — push closure + args + CallClosure.
            other => {
                self.compile_expr(other, false);
                for a in args { self.compile_expr(a, false); }
                self.emit(Op::CallClosure { arity: args.len() as u16, node_id_idx });
            }
        }
    }

    fn compile_binop(&mut self, op: &str, lhs: &a::CExpr, rhs: &a::CExpr) {
        // #461 typed lowering: if we can statically prove both
        // operands are the same numeric type, emit the typed
        // primitive (`IntAdd` / `FloatAdd`) instead of the
        // polymorphic `NumAdd` that runtime-matches on operand
        // shape. The fast path skips one match per arithmetic op
        // *and* unblocks downstream peephole fusions (slice 1)
        // that scan for typed primitives. Conservative fallback
        // to the polymorphic op when either side classifies as
        // `Unknown`, so correctness for `Float` / mixed code is
        // unchanged.
        let lhs_ty = self.classify_expr(lhs);
        let rhs_ty = self.classify_expr(rhs);
        let typed = match (lhs_ty, rhs_ty) {
            (NumTy::Int, NumTy::Int) => NumTy::Int,
            (NumTy::Float, NumTy::Float) => NumTy::Float,
            _ => NumTy::Unknown,
        };
        self.compile_expr(lhs, false);
        self.compile_expr(rhs, false);
        match (op, typed) {
            ("+",  NumTy::Int)     => self.emit(Op::IntAdd),
            ("+",  NumTy::Float)   => self.emit(Op::FloatAdd),
            ("+",  NumTy::Unknown) => self.emit(Op::NumAdd),
            ("-",  NumTy::Int)     => self.emit(Op::IntSub),
            ("-",  NumTy::Float)   => self.emit(Op::FloatSub),
            ("-",  NumTy::Unknown) => self.emit(Op::NumSub),
            ("*",  NumTy::Int)     => self.emit(Op::IntMul),
            ("*",  NumTy::Float)   => self.emit(Op::FloatMul),
            ("*",  NumTy::Unknown) => self.emit(Op::NumMul),
            ("/",  NumTy::Int)     => self.emit(Op::IntDiv),
            ("/",  NumTy::Float)   => self.emit(Op::FloatDiv),
            ("/",  NumTy::Unknown) => self.emit(Op::NumDiv),
            // Int has %; Float doesn't (NumMod will reject at runtime).
            ("%",  NumTy::Int)     => self.emit(Op::IntMod),
            ("%",  _)              => self.emit(Op::NumMod),
            ("==", NumTy::Int)     => self.emit(Op::IntEq),
            ("==", NumTy::Float)   => self.emit(Op::FloatEq),
            ("==", NumTy::Unknown) => self.emit(Op::NumEq),
            ("!=", NumTy::Int)     => { self.emit(Op::IntEq);   self.emit(Op::BoolNot); }
            ("!=", NumTy::Float)   => { self.emit(Op::FloatEq); self.emit(Op::BoolNot); }
            ("!=", NumTy::Unknown) => { self.emit(Op::NumEq);   self.emit(Op::BoolNot); }
            ("<",  NumTy::Int)     => self.emit(Op::IntLt),
            ("<",  NumTy::Float)   => self.emit(Op::FloatLt),
            ("<",  NumTy::Unknown) => self.emit(Op::NumLt),
            ("<=", NumTy::Int)     => self.emit(Op::IntLe),
            ("<=", NumTy::Float)   => self.emit(Op::FloatLe),
            ("<=", NumTy::Unknown) => self.emit(Op::NumLe),
            (">",  NumTy::Int)     => { self.emit_swap_top2(); self.emit(Op::IntLt); }
            (">",  NumTy::Float)   => { self.emit_swap_top2(); self.emit(Op::FloatLt); }
            (">",  NumTy::Unknown) => { self.emit_swap_top2(); self.emit(Op::NumLt); }
            (">=", NumTy::Int)     => { self.emit_swap_top2(); self.emit(Op::IntLe); }
            (">=", NumTy::Float)   => { self.emit_swap_top2(); self.emit(Op::FloatLe); }
            (">=", NumTy::Unknown) => { self.emit_swap_top2(); self.emit(Op::NumLe); }
            ("and", _) => self.emit(Op::BoolAnd),
            ("or",  _) => self.emit(Op::BoolOr),
            (other, _) => panic!("unknown binop: {other:?}"),
        }
    }

    /// Classify an expression's static numeric type for #461 typed
    /// lowering. Strictly conservative: only returns `Int` / `Float`
    /// when the type is locally derivable from a literal, an
    /// already-classified local, or a binary op on two same-typed
    /// operands. Everything else (function calls, field access,
    /// match expressions, ...) falls back to `Unknown` and the
    /// polymorphic NumAdd-family op.
    fn classify_expr(&self, e: &a::CExpr) -> NumTy {
        match e {
            a::CExpr::Literal { value: a::CLit::Int { .. } } => NumTy::Int,
            a::CExpr::Literal { value: a::CLit::Float { .. } } => NumTy::Float,
            a::CExpr::Var { name } =>
                self.local_types.get(name).copied().unwrap_or(NumTy::Unknown),
            a::CExpr::BinOp { op, lhs, rhs } => {
                // Numeric ops preserve the operand type (Int+Int=Int,
                // Float+Float=Float). Comparison/logical ops yield
                // Bool, not a numeric type — return Unknown.
                let is_numeric = matches!(op.as_str(), "+" | "-" | "*" | "/" | "%");
                if !is_numeric { return NumTy::Unknown; }
                match (self.classify_expr(lhs), self.classify_expr(rhs)) {
                    (NumTy::Int, NumTy::Int) => NumTy::Int,
                    (NumTy::Float, NumTy::Float) => NumTy::Float,
                    _ => NumTy::Unknown,
                }
            }
            a::CExpr::UnaryOp { op, expr } if op == "-" => self.classify_expr(expr),
            // #461 slice 7: `r.field` access where `r` is a local
            // bound from a record literal. Reads the per-local
            // field-type map populated at the let-binding site.
            // Unknown otherwise (record argument with `:: R`
            // annotation, helper-returned record, etc.) — those
            // would need type-alias resolution to classify.
            a::CExpr::FieldAccess { value, field } => {
                if let a::CExpr::Var { name } = value.as_ref() {
                    if let Some(ftypes) = self.local_record_field_types.get(name) {
                        return ftypes.get(field).copied().unwrap_or(NumTy::Unknown);
                    }
                }
                NumTy::Unknown
            }
            // Let-expressions: the let-binding mutates `local_types`
            // *during* compile_expr; classifying ahead of time would
            // require simulating that. Conservative fallback.
            _ => NumTy::Unknown,
        }
    }

    fn emit_swap_top2(&mut self) {
        let a = self.alloc_local("__swap_a");
        let b = self.alloc_local("__swap_b");
        self.emit(Op::StoreLocal(b));
        self.emit(Op::StoreLocal(a));
        self.emit(Op::LoadLocal(b));
        self.emit(Op::LoadLocal(a));
    }

    fn compile_match(&mut self, scrutinee: &a::CExpr, arms: &[a::Arm], tail: bool) {
        self.compile_expr(scrutinee, false);
        let scrut_slot = self.alloc_local("__scrut");
        self.emit(Op::StoreLocal(scrut_slot));

        let mut end_jumps: Vec<usize> = Vec::new();
        for arm in arms {
            let arm_start_locals = self.next_local;
            let arm_start_locals_map = self.locals.clone();

            self.emit(Op::LoadLocal(scrut_slot));
            let mut bindings: Vec<(String, u16)> = Vec::new();
            let fail_jumps: Vec<usize> = self.compile_pattern_test(&arm.pattern, &mut bindings);

            self.compile_expr(&arm.body, tail);
            let j_end = self.code.len();
            self.emit(Op::Jump(0));
            end_jumps.push(j_end);

            let fail_target = self.code.len() as i32;
            for j in fail_jumps {
                // #337: PConstructor patterns now register an
                // unconditional `Op::Jump` for the failure path
                // (alongside the existing `Op::JumpIfNot` from
                // PLiteral / nested constructor tests). Patch
                // either shape.
                match &mut self.code[j] {
                    Op::JumpIfNot(off) => *off = fail_target - (j as i32 + 1),
                    Op::Jump(off)      => *off = fail_target - (j as i32 + 1),
                    _ => {}
                }
            }
            self.next_local = arm_start_locals;
            self.locals = arm_start_locals_map;
        }
        let panic_msg_idx = self.pool.str("non-exhaustive match");
        self.emit(Op::Panic(panic_msg_idx));

        let end_target = self.code.len() as i32;
        for j in end_jumps {
            if let Op::Jump(off) = &mut self.code[j] {
                *off = end_target - (j as i32 + 1);
            }
        }
    }

    fn compile_pattern_test(&mut self, p: &a::Pattern, bindings: &mut Vec<(String, u16)>) -> Vec<usize> {
        let mut fails = Vec::new();
        match p {
            a::Pattern::PWild => { self.emit(Op::Pop); }
            a::Pattern::PVar { name } => {
                let slot = self.alloc_local(name);
                self.emit(Op::StoreLocal(slot));
                bindings.push((name.clone(), slot));
            }
            a::Pattern::PLiteral { value } => {
                self.compile_lit(value);
                match value {
                    a::CLit::Str { .. } => self.emit(Op::StrEq),
                    a::CLit::Bytes { .. } => self.emit(Op::BytesEq),
                    // Typed-lowering for numeric literal patterns
                    // (#461 slice 5 prerequisite). The pattern only
                    // reaches its test when the scrutinee has the
                    // literal's type (the type checker rejects
                    // mismatches), so emit the type-specific Eq.
                    // The body_hash decoder lowers IntEq/FloatEq to
                    // NumEq at hash time so closure identity (#222)
                    // is unchanged. Enables slice 5's
                    // `LoadLocal + PushConst + IntEq + JumpIfNot`
                    // peephole to fire on pattern-match arm tests.
                    a::CLit::Int { .. } => self.emit(Op::IntEq),
                    a::CLit::Float { .. } => self.emit(Op::FloatEq),
                    _ => self.emit(Op::NumEq),
                }
                let j = self.code.len();
                self.emit(Op::JumpIfNot(0));
                fails.push(j);
            }
            a::Pattern::PConstructor { name, args } => {
                let name_idx = self.pool.variant(name);
                // #337: the failure path must drop the duplicated
                // scrutinee so subsequent match arms see a clean
                // stack. The previous shape
                //   Dup; TestVariant; JumpIfNot(fail);
                // left `[scrut]` on the stack at the fail target,
                // poisoning later arms — e.g. a wildcard `_` arm
                // whose body referenced an unrelated value would
                // pop the leaked scrutinee instead of its own value.
                //
                // New shape: branch on success, fall through to a
                // failure cleanup that pops the dup'd scrutinee
                // before jumping. The registered fail-jump is an
                // unconditional `Op::Jump`; `compile_match`'s patch
                // loop accepts both `JumpIfNot` and `Jump`.
                self.emit(Op::Dup);                   // [scrut, scrut]
                self.emit(Op::TestVariant(name_idx)); // [scrut, Bool]
                let j_success = self.code.len();
                self.emit(Op::JumpIf(0));             // pop Bool. success → [scrut]
                self.emit(Op::Pop);                   // failure cleanup: [scrut] → []
                let j_fail = self.code.len();
                self.emit(Op::Jump(0));               // → fail target with []
                fails.push(j_fail);
                let success_target = self.code.len() as i32;
                if let Op::JumpIf(off) = &mut self.code[j_success] {
                    *off = success_target - (j_success as i32 + 1);
                }
                if args.is_empty() {
                    self.emit(Op::Pop);
                } else if args.len() == 1 {
                    self.emit(Op::GetVariantArg(0));
                    let sub_fails = self.compile_pattern_test(&args[0], bindings);
                    fails.extend(sub_fails);
                } else {
                    let slot = self.alloc_local("__variant");
                    self.emit(Op::StoreLocal(slot));
                    for (i, arg) in args.iter().enumerate() {
                        self.emit(Op::LoadLocal(slot));
                        self.emit(Op::GetVariantArg(i as u16));
                        let sub_fails = self.compile_pattern_test(arg, bindings);
                        fails.extend(sub_fails);
                    }
                }
            }
            a::Pattern::PRecord { fields } => {
                let slot = self.alloc_local("__record");
                self.emit(Op::StoreLocal(slot));
                for f in fields {
                    self.emit(Op::LoadLocal(slot));
                    let name_idx = self.pool.field(&f.name);
                    let site_idx = self.field_get_sites;
                    self.field_get_sites += 1;
                    self.emit(Op::GetField { name_idx, site_idx });
                    let sub_fails = self.compile_pattern_test(&f.pattern, bindings);
                    fails.extend(sub_fails);
                }
            }
            a::Pattern::PTuple { items } => {
                let slot = self.alloc_local("__tuple");
                self.emit(Op::StoreLocal(slot));
                for (i, item) in items.iter().enumerate() {
                    self.emit(Op::LoadLocal(slot));
                    self.emit(Op::GetElem(i as u16));
                    let sub_fails = self.compile_pattern_test(item, bindings);
                    fails.extend(sub_fails);
                }
            }
        }
        fails
    }

    /// Compile a Lambda: collect free variables that resolve to outer-scope
    /// locals, register a synthetic function, emit MakeClosure with the
    /// captured values pushed in order.
    fn compile_lambda(&mut self, params: &[a::Param], body: &a::CExpr) {
        // Free vars = vars referenced in body that aren't bound locally.
        let mut bound: std::collections::HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut frees: Vec<String> = Vec::new();
        free_vars(body, &mut bound, &mut frees);

        // Filter to those that are in the enclosing locals (captures).
        // Don't exclude names that *also* exist in `function_names`:
        // if the name is in `locals`, the local shadows the global
        // within this scope, and the lambda needs to capture the
        // local's value, not the global fn. (#339) Names that are
        // ONLY in `function_names` (no local) stay external — the
        // lambda's body resolves them at call time, same as the
        // enclosing fn would.
        let captures: Vec<String> = frees.into_iter()
            .filter(|n| self.locals.contains_key(n))
            .collect();

        // Allocate a fresh fn_id by appending a placeholder Function.
        let fn_id = self.next_fn_id.len() as u32;
        self.next_fn_id.push(Function {
            name: format!("__lambda_{fn_id}"),
            arity: (captures.len() + params.len()) as u16,
            locals_count: 0,
            code: Vec::new(),
            effects: Vec::new(),
            // See #222: filled in at the end of the compile pass.
            body_hash: crate::program::ZERO_BODY_HASH,
            // Lambdas don't carry refinements at the surface today
            // (closure params don't accept `Type{x | ...}` syntax in
            // the parser). #209 stays focused on top-level fn decls;
            // closure-param refinements are a follow-up.
            refinements: Vec::new(),
            // Lambda body hasn't been compiled yet; filled in by the
            // deferred lambda-compile pass after FnCompiler walks it.
            field_ic_sites: 0,
        });

        // Emit code at the lambda site: load each captured local, then MakeClosure.
        for c in &captures {
            let slot = *self.locals.get(c).expect("free var must be in scope");
            self.emit(Op::LoadLocal(slot));
        }
        self.emit(Op::MakeClosure { fn_id, capture_count: captures.len() as u16 });

        // Queue the body for later compilation.
        self.pending_lambdas.push(PendingLambda {
            fn_id,
            capture_names: captures,
            params: params.to_vec(),
            body: body.clone(),
        });
    }

    /// Higher-order stdlib ops on Result/Option whose function arg is a
    /// closure. Emit inline: pattern-match on the variant, invoke the
    /// closure when applicable, return wrapped result.
    fn try_emit_higher_order(
        &mut self,
        module: &str,
        op: &str,
        args: &[a::CExpr],
        node_id_idx: u32,
    ) -> bool {
        match (module, op) {
            ("result", "map") => self.emit_variant_map(args, "Ok", true),
            ("result", "and_then") => self.emit_variant_map(args, "Ok", false),
            ("result", "map_err") => self.emit_variant_map(args, "Err", true),
            ("result", "or_else") => self.emit_variant_or_else(args, "Err", 1),
            ("option", "map") => self.emit_variant_map(args, "Some", true),
            ("option", "and_then") => self.emit_variant_map(args, "Some", false),
            ("option", "or_else") => self.emit_variant_or_else(args, "None", 0),
            ("option", "unwrap_or_else") => self.emit_option_unwrap_or_else(args),
            ("result", "unwrap_or_else") => self.emit_result_unwrap_or_else(args),
            ("list", "map") => self.emit_list_map(args),
            ("list", "par_map") => self.emit_list_par_map(args),
            ("list", "sort_by") => self.emit_list_sort_by(args),
            ("list", "filter") => self.emit_list_filter(args),
            ("list", "fold") => self.emit_list_fold(args),
            ("iter", "from_list") => self.emit_iter_from_list(args),
            ("iter", "unfold")    => self.emit_iter_unfold(args),
            ("iter", "next")      => self.emit_iter_next(args),
            ("iter", "is_empty")  => self.emit_iter_is_empty(args),
            ("iter", "count")     => self.emit_iter_count(args),
            ("iter", "take")      => self.emit_iter_take(args),
            ("iter", "skip")      => self.emit_iter_skip(args),
            ("iter", "to_list")   => self.emit_iter_to_list(args),
            ("iter", "collect")   => self.emit_iter_to_list(args),
            ("iter", "map")       => self.emit_iter_map(args),
            ("iter", "filter")    => self.emit_iter_filter(args),
            ("iter", "fold")      => self.emit_iter_fold(args),
            ("map", "fold") => self.emit_map_fold(args, node_id_idx),
            ("flow", "sequential") => self.emit_flow_sequential(args),
            ("flow", "branch") => self.emit_flow_branch(args),
            ("flow", "retry") => self.emit_flow_retry(args),
            ("flow", "retry_with_backoff") => self.emit_flow_retry_with_backoff(args),
            ("flow", "parallel") => self.emit_flow_parallel(args),
            ("flow", "parallel_list") => self.emit_flow_parallel_list(args),
            _ => return false,
        }
        true
    }

    /// `list.map(xs, f)` — native map op (#464). Pushes `xs` then `f`
    /// and emits a single `Op::ListMap`. The previous inlined loop
    /// re-`LoadLocal`'d (cloned) the whole input and accumulator lists
    /// each iteration — O(n²); the native op owns the list and builds
    /// the result with one pre-sized allocation.
    fn emit_list_map(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false); // xs
        self.compile_expr(&args[1], false); // f
        let nid = self.pool.node_id("n_list_map");
        self.emit(Op::ListMap { node_id_idx: nid });
    }

    /// `list.par_map(xs, f)` (#305 slice 1). Pushes `xs` and `f`,
    /// then emits a single `Op::ParallelMap` — the VM applies `f`
    /// to each element on OS-thread tasks, capped by
    /// `LEX_PAR_MAX_CONCURRENCY`. Returns the result list in input
    /// order.
    fn emit_list_par_map(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_list_par_map");
        self.emit(Op::ParallelMap { node_id_idx: nid });
    }

    /// `list.sort_by(xs, f)` (#338). Pushes `xs` and the key-fn
    /// `f`, then emits a single `Op::SortByKey` — the VM invokes
    /// `f` on each element to derive a sortable key, stable-sorts
    /// by key, and returns the values in sorted order. Keys must
    /// resolve to `Int` / `Float` / `Str`; mixed-type pairs are
    /// treated as equal by the comparator (preserving insertion
    /// order via the stable sort).
    fn emit_list_sort_by(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_list_sort_by");
        self.emit(Op::SortByKey { node_id_idx: nid });
    }

    /// `list.filter(xs, pred)` — native filter op (#464). Same
    /// rationale as `emit_list_map`.
    fn emit_list_filter(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false); // xs
        self.compile_expr(&args[1], false); // pred
        let nid = self.pool.node_id("n_list_filter");
        self.emit(Op::ListFilter { node_id_idx: nid });
    }

    /// `list.fold(xs, init, f)` — native left-fold op (#464). Same
    /// rationale as `emit_list_map`. Stack: `[xs, init, f]`.
    fn emit_list_fold(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false); // xs
        self.compile_expr(&args[1], false); // init
        self.compile_expr(&args[2], false); // f
        let nid = self.pool.node_id("n_list_fold");
        self.emit(Op::ListFold { node_id_idx: nid });
    }

    // ── Iter[T] operations (#364) ─────────────────────────────────────────
    // Internal representation: `Value::Variant("__IterEager", [list, idx])`
    // for the eager form (a List backing store + Int cursor) and
    // `Value::Variant("__IterLazy", [seed, step_closure])` for the lazy form
    // produced by `iter.unfold` (#376). Both are tagged variants so each op
    // can `TestVariant` at runtime to dispatch. The names start with `__` so
    // they can't be written by user code (uppercase ASCII-letter is required
    // for constructor names, and the underscores keep them out of the
    // user-namespace by convention).

    /// `iter.from_list(xs)` — wrap a list in an eager iterator at position 0.
    fn emit_iter_from_list(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: v, arity: 2 });
    }

    /// `iter.next(it)` — advance one step; returns `Option[(T, Iter[T])]`.
    ///
    /// Dispatches on the iter's variant tag:
    /// - `__IterLazy(seed, step)` (#376) → invoke `step(seed)`. On
    ///   `Some((t, s'))` wrap as `Some((t, __IterLazy(s', step)))`; on
    ///   `None` propagate `None`. The seed advances forward each call.
    /// - `__IterCursor(handle)` (#379) → effect-call `sql.cursor_next(handle)`
    ///   which returns `Option[T]`. On `Some(row)` wrap as
    ///   `Some((row, __IterCursor(handle)))`; on `None` propagate. Handle
    ///   stays stable across calls — state is server-side / mpsc-buffered.
    /// - `__IterEager(list, idx)` → existing positional cursor.
    fn emit_iter_next(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__in_it");
        self.emit(Op::StoreLocal(it));

        // Dispatch: TestVariant pops; we Dup to keep the iter around.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::Dup);
        let lazy_name = self.pool.variant("__IterLazy");
        self.emit(Op::TestVariant(lazy_name));
        let j_to_check_cursor = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // ── lazy path ────────────────────────────────────────────────
        // The Dup'd iter is on stack but we've consumed it via TestVariant,
        // so reload from the local.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0)); // seed
        let seed = self.alloc_local("__in_seed");
        self.emit(Op::StoreLocal(seed));

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(1)); // step closure
        let step = self.alloc_local("__in_step");
        self.emit(Op::StoreLocal(step));

        // Call step(seed) → Option[(T, S)].
        let nid_lazy = self.pool.node_id("n_iter_next_lazy");
        self.emit(Op::LoadLocal(step));
        self.emit(Op::LoadLocal(seed));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid_lazy });
        let opt = self.alloc_local("__in_opt");
        self.emit(Op::StoreLocal(opt));

        // If `step` returned None, propagate it directly.
        self.emit(Op::LoadLocal(opt));
        let some_name = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_name));
        let j_lazy_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((t, new_seed)) — extract the inner tuple, repackage as
        // Some((t, __IterLazy(new_seed, step))) so the next call advances.
        self.emit(Op::LoadLocal(opt));
        self.emit(Op::GetVariantArg(0));     // (t, new_seed)
        let pair = self.alloc_local("__in_pair");
        self.emit(Op::StoreLocal(pair));

        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(0));           // t
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(1));           // new_seed
        self.emit(Op::LoadLocal(step));      // step closure
        let lazy_v = self.pool.variant("__IterLazy");
        self.emit(Op::MakeVariant { name_idx: lazy_v, arity: 2 }); // __IterLazy(new_seed, step)
        self.emit(Op::MakeTuple(2));         // (t, new_iter)
        let some_v = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_v, arity: 1 });
        let j_after_lazy = self.code.len();
        self.emit(Op::Jump(0));

        // Lazy → None: just forward the None.
        let none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_lazy_none] {
            *off = none_t - (j_lazy_none as i32 + 1);
        }
        let none_v = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_v, arity: 0 });
        let j_after_lazy_none = self.code.len();
        self.emit(Op::Jump(0));

        // ── cursor path (#379) ───────────────────────────────────────
        let cursor_check_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_check_cursor] {
            *off = cursor_check_t - (j_to_check_cursor as i32 + 1);
        }

        self.emit(Op::LoadLocal(it));
        self.emit(Op::Dup);
        let cursor_name = self.pool.variant("__IterCursor");
        self.emit(Op::TestVariant(cursor_name));
        let j_to_eager = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Cursor path: extract handle, effect-call sql.cursor_next(handle).
        // The handler returns Option[T] directly. We then wrap as
        // Some((T, __IterCursor(handle))) or forward None.
        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0));     // handle
        let handle = self.alloc_local("__in_handle");
        self.emit(Op::StoreLocal(handle));

        let kind_idx = self.pool.str("sql");
        let op_idx = self.pool.str("cursor_next");
        let nid_cursor = self.pool.node_id("n_iter_next_cursor");
        self.emit(Op::LoadLocal(handle));
        self.emit(Op::EffectCall {
            kind_idx,
            op_idx,
            arity: 1,
            node_id_idx: nid_cursor,
        });
        let cur_opt = self.alloc_local("__in_cur_opt");
        self.emit(Op::StoreLocal(cur_opt));

        self.emit(Op::LoadLocal(cur_opt));
        let some_c = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_c));
        let j_cursor_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some(row): build Some((row, __IterCursor(handle)))
        self.emit(Op::LoadLocal(cur_opt));
        self.emit(Op::GetVariantArg(0));     // row
        self.emit(Op::LoadLocal(handle));
        let cursor_v = self.pool.variant("__IterCursor");
        self.emit(Op::MakeVariant { name_idx: cursor_v, arity: 1 });
        self.emit(Op::MakeTuple(2));         // (row, __IterCursor(handle))
        let some_c2 = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_c2, arity: 1 });
        let j_after_cursor = self.code.len();
        self.emit(Op::Jump(0));

        // Cursor → None
        let cursor_none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_cursor_none] {
            *off = cursor_none_t - (j_cursor_none as i32 + 1);
        }
        let none_c = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_c, arity: 0 });
        let j_after_cursor_none = self.code.len();
        self.emit(Op::Jump(0));

        // ── eager path ───────────────────────────────────────────────
        let eager_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_eager] {
            *off = eager_t - (j_to_eager as i32 + 1);
        }

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__in_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it));
        self.emit(Op::GetVariantArg(1));
        let idx = self.alloc_local("__in_idx");
        self.emit(Op::StoreLocal(idx));

        // if idx < len(list)
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_eager_else = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((item, __IterEager(list, idx+1)))
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::GetListElemDyn);

        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(idx));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
        self.emit(Op::MakeTuple(2));
        let some_e = self.pool.variant("Some");
        self.emit(Op::MakeVariant { name_idx: some_e, arity: 1 });
        let j_after_eager = self.code.len();
        self.emit(Op::Jump(0));

        // Eager → None
        let eager_none_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_eager_else] {
            *off = eager_none_t - (j_eager_else as i32 + 1);
        }
        let none_e = self.pool.variant("None");
        self.emit(Op::MakeVariant { name_idx: none_e, arity: 0 });

        // Converge all paths.
        let end = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_after_lazy] {
            *off = end - (j_after_lazy as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_lazy_none] {
            *off = end - (j_after_lazy_none as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_cursor] {
            *off = end - (j_after_cursor as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_cursor_none] {
            *off = end - (j_after_cursor_none as i32 + 1);
        }
        if let Op::Jump(off) = &mut self.code[j_after_eager] {
            *off = end - (j_after_eager as i32 + 1);
        }
    }

    /// `iter.unfold(seed, step)` — lazy iterator that calls `step(seed)` on
    /// each `iter.next` and threads the new seed forward. Internal value
    /// shape: `__IterLazy(seed, step)`. Step has type `(S) -> Option[(T, S)]`;
    /// returning `None` ends the iteration (#376).
    fn emit_iter_unfold(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false); // seed
        self.compile_expr(&args[1], false); // step
        let lazy = self.pool.variant("__IterLazy");
        self.emit(Op::MakeVariant { name_idx: lazy, arity: 2 });
    }

    /// `iter.is_empty(it)` — true iff no further element. v1 supports the
    /// eager form O(1); on a lazy iter the seed sits in slot 0 and is not a
    /// List, so the VM trips on `GetListLen` rather than returning a wrong
    /// answer. Callers needing lazy support should materialize with
    /// `iter.to_list` first or call `iter.next` and pattern-match.
    fn emit_iter_is_empty(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__ie_it");
        self.emit(Op::StoreLocal(it));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1)); // idx
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0)); // list
        self.emit(Op::GetListLen);                                     // len
        self.emit(Op::IntLt);                                          // idx < len
        self.emit(Op::BoolNot);                                        // NOT(idx < len)
    }

    /// `iter.count(it)` — number of remaining elements (v1: eager-only).
    fn emit_iter_count(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__ic_it");
        self.emit(Op::StoreLocal(it));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        self.emit(Op::GetListLen);                                     // len
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1)); // idx
        self.emit(Op::IntSub);                                         // len - idx
    }

    /// `iter.take(it, n)` — collect up to n elements, return as new Iter.
    fn emit_iter_take(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__itk_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let n    = self.alloc_local("__itk_n");
        self.emit(Op::StoreLocal(n));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__itk_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__itk_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__itk_out");
        self.emit(Op::StoreLocal(out));

        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let cnt  = self.alloc_local("__itk_cnt");
        self.emit(Op::StoreLocal(cnt));

        let loop_top = self.code.len();

        // while cnt < n
        self.emit(Op::LoadLocal(cnt));
        self.emit(Op::LoadLocal(n));
        self.emit(Op::IntLt);
        let j_exit_n = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // AND i < len(list)
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit_l = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // out = out ++ [list[i]]
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        let one = self.pool.int(1);
        // i = i + 1
        self.emit(Op::LoadLocal(i));
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));
        // cnt = cnt + 1
        self.emit(Op::LoadLocal(cnt));
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(cnt));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit_n] { *off = exit_t - (j_exit_n as i32 + 1); }
        if let Op::JumpIfNot(off) = &mut self.code[j_exit_l] { *off = exit_t - (j_exit_l as i32 + 1); }

        // return new __IterEager(out, 0)
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.skip(it, n)` — advance cursor by n (or to end), return new Iter.
    fn emit_iter_skip(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__isk_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let n    = self.alloc_local("__isk_n");
        self.emit(Op::StoreLocal(n));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__isk_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let idx  = self.alloc_local("__isk_idx");
        self.emit(Op::StoreLocal(idx));

        // raw = idx + n
        self.emit(Op::LoadLocal(idx));
        self.emit(Op::LoadLocal(n));
        self.emit(Op::IntAdd);
        let raw  = self.alloc_local("__isk_raw");
        self.emit(Op::StoreLocal(raw));

        // new_idx = if raw < len then raw else len
        self.emit(Op::LoadLocal(raw));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_use_raw = self.code.len();
        self.emit(Op::JumpIf(0));

        // use len
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // use raw
        let raw_t = self.code.len() as i32;
        if let Op::JumpIf(off) = &mut self.code[j_use_raw] { *off = raw_t - (j_use_raw as i32 + 1); }
        self.emit(Op::LoadLocal(raw));

        let end_t = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] { *off = end_t - (j_end as i32 + 1); }

        // new_idx on stack; build new __IterEager(list, new_idx)
        let new_idx = self.alloc_local("__isk_ni");
        self.emit(Op::StoreLocal(new_idx));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(new_idx));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.to_list(it)` — materialise remaining elements into a List.
    ///
    /// Dispatches on the iter variant (#376):
    /// - `__IterLazy`: repeatedly call `step(seed)`; on `Some((t, s'))` append
    ///   `t` and continue with `s'`; on `None` stop. May hang on truly
    ///   infinite producers — that's documented as a v1 limitation, the
    ///   step-limit-protected caller is what catches misuse.
    /// - `__IterEager`: slice the backing list from `idx` onward (O(n) walk).
    fn emit_iter_to_list(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it = self.alloc_local("__itl_it");
        self.emit(Op::StoreLocal(it));

        // Build the output list up-front, shared across both paths.
        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__itl_out");
        self.emit(Op::StoreLocal(out));

        // Dispatch on variant tag.
        self.emit(Op::LoadLocal(it));
        let lazy_name = self.pool.variant("__IterLazy");
        self.emit(Op::TestVariant(lazy_name));
        let j_to_eager = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // ── lazy path ─────────────────────────────────────────────────
        // seed and step closure live in locals; we update seed each iteration.
        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let seed = self.alloc_local("__itl_seed");
        self.emit(Op::StoreLocal(seed));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let step = self.alloc_local("__itl_step");
        self.emit(Op::StoreLocal(step));

        let lazy_loop = self.code.len();
        let nid_lazy = self.pool.node_id("n_iter_to_list_lazy");
        self.emit(Op::LoadLocal(step));
        self.emit(Op::LoadLocal(seed));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid_lazy });
        let opt = self.alloc_local("__itl_opt");
        self.emit(Op::StoreLocal(opt));

        // If None, drop out of the lazy loop.
        self.emit(Op::LoadLocal(opt));
        let some_name = self.pool.variant("Some");
        self.emit(Op::TestVariant(some_name));
        let j_lazy_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some((t, new_seed)): append t to out, replace seed.
        self.emit(Op::LoadLocal(opt));
        self.emit(Op::GetVariantArg(0));
        let pair = self.alloc_local("__itl_pair");
        self.emit(Op::StoreLocal(pair));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(pair)); self.emit(Op::GetElem(0));
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(pair)); self.emit(Op::GetElem(1));
        self.emit(Op::StoreLocal(seed));

        let jback_lazy = self.code.len();
        self.emit(Op::Jump((lazy_loop as i32) - (jback_lazy as i32 + 1)));

        let lazy_exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_lazy_exit] {
            *off = lazy_exit_t - (j_lazy_exit as i32 + 1);
        }
        let j_after_lazy = self.code.len();
        self.emit(Op::Jump(0));

        // ── eager path ────────────────────────────────────────────────
        let eager_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_to_eager] {
            *off = eager_t - (j_to_eager as i32 + 1);
        }

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__itl_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i = self.alloc_local("__itl_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_t - (j_exit as i32 + 1);
        }

        // Converge: lazy path falls through here too.
        let converge = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_after_lazy] {
            *off = converge - (j_after_lazy as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `iter.map(it, f)` — apply `f` to each remaining element; returns new Iter.
    fn emit_iter_map(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__im_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let f    = self.alloc_local("__im_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__im_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__im_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__im_out");
        self.emit(Op::StoreLocal(out));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        let nid = self.pool.node_id("n_iter_map");
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }

        let zero = self.pool.int(0);
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.filter(it, pred)` — keep elements where pred is true; returns new Iter.
    fn emit_iter_filter(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__if_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let f    = self.alloc_local("__if_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__if_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__if_i");
        self.emit(Op::StoreLocal(i));

        self.emit(Op::MakeList(0));
        let out  = self.alloc_local("__if_out");
        self.emit(Op::StoreLocal(out));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // elem := list[i]
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        let x    = self.alloc_local("__if_x");
        self.emit(Op::StoreLocal(x));

        let nid = self.pool.node_id("n_iter_filter");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(x));
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        let skip_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] { *off = skip_t - (j_skip as i32 + 1); }

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }

        let zero = self.pool.int(0);
        self.emit(Op::LoadLocal(out));
        self.emit(Op::PushConst(zero));
        let eager_v = self.pool.variant("__IterEager");
        self.emit(Op::MakeVariant { name_idx: eager_v, arity: 2 });
    }

    /// `iter.fold(it, init, f)` — left fold over remaining elements.
    fn emit_iter_fold(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        let it   = self.alloc_local("__ifo_it");
        self.emit(Op::StoreLocal(it));

        self.compile_expr(&args[1], false);
        let acc  = self.alloc_local("__ifo_acc");
        self.emit(Op::StoreLocal(acc));

        self.compile_expr(&args[2], false);
        let f    = self.alloc_local("__ifo_f");
        self.emit(Op::StoreLocal(f));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(0));
        let list = self.alloc_local("__ifo_list");
        self.emit(Op::StoreLocal(list));

        self.emit(Op::LoadLocal(it)); self.emit(Op::GetVariantArg(1));
        let i    = self.alloc_local("__ifo_i");
        self.emit(Op::StoreLocal(i));

        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(list)); self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        let nid = self.pool.node_id("n_iter_fold");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(acc));
        self.emit(Op::LoadLocal(list));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 2, node_id_idx: nid });
        self.emit(Op::StoreLocal(acc));

        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        let jback = self.code.len();
        self.emit(Op::Jump((loop_top as i32) - (jback as i32 + 1)));

        let exit_t = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] { *off = exit_t - (j_exit as i32 + 1); }
        self.emit(Op::LoadLocal(acc));
    }

    /// `map.fold(m, init, f)` — left fold over `Map[K, V]` entries with a
    /// three-arg combiner `f(acc, k, v)`. Iteration order matches
    /// `map.entries` (BTreeMap-sorted by key). Materializes the entry
    /// list once via the runtime's `("map", "entries")` op, then runs
    /// the same inline loop as `list.fold`.
    fn emit_map_fold(&mut self, args: &[a::CExpr], node_id_idx: u32) {
        // xs := map.entries(m)
        self.compile_expr(&args[0], false);
        let map_kind = self.pool.str("map");
        let entries_op = self.pool.str("entries");
        self.emit(Op::EffectCall {
            kind_idx: map_kind,
            op_idx: entries_op,
            arity: 1,
            node_id_idx,
        });
        let xs = self.alloc_local("__mf_xs");
        self.emit(Op::StoreLocal(xs));

        // acc := init
        self.compile_expr(&args[1], false);
        let acc = self.alloc_local("__mf_acc");
        self.emit(Op::StoreLocal(acc));

        // f := <closure>
        self.compile_expr(&args[2], false);
        let f = self.alloc_local("__mf_f");
        self.emit(Op::StoreLocal(f));

        // i := 0
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__mf_i");
        self.emit(Op::StoreLocal(i));

        // loop_top: while i < len(xs)
        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // pair := xs[i]
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        let pair = self.alloc_local("__mf_pair");
        self.emit(Op::StoreLocal(pair));

        // acc := f(acc, pair.0, pair.1)
        let nid = self.pool.node_id("n_map_fold");
        self.emit(Op::LoadLocal(f));
        self.emit(Op::LoadLocal(acc));
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(0));
        self.emit(Op::LoadLocal(pair));
        self.emit(Op::GetElem(1));
        self.emit(Op::CallClosure { arity: 3, node_id_idx: nid });
        self.emit(Op::StoreLocal(acc));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(acc));
    }

    /// Inline pattern: `<module>.map(v, f)` and friends.
    /// `wrap_with`: variant tag whose payload triggers the call (Ok / Some / Err).
    /// `wrap_result`: if true, wrap the closure's result back in `wrap_with`
    /// (map shape); if false, expect the closure to return a wrapped value
    /// itself (and_then shape).
    fn emit_variant_map(
        &mut self,
        args: &[a::CExpr],
        wrap_with: &str,
        wrap_result: bool,
    ) {
        // args[0] = the wrapped value (Result/Option), args[1] = closure
        let wrap_idx = self.pool.variant(wrap_with);

        // Compile and store the value into a local, evaluate closure on top of stack.
        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__hov");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__hof");
        self.emit(Op::StoreLocal(f_slot));

        // Stack discipline:
        //   load val ⇒ [v]
        //   dup     ⇒ [v, v]
        //   test    ⇒ [v, Bool]
        //   jumpifnot ⇒ [v]
        // Both branches end with [v] before the branch body.
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(wrap_idx));
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Matched arm: extract payload, call closure on it.
        self.emit(Op::GetVariantArg(0));
        let arg_slot = self.alloc_local("__hov_arg");
        self.emit(Op::StoreLocal(arg_slot));
        self.emit(Op::LoadLocal(f_slot));
        self.emit(Op::LoadLocal(arg_slot));
        let nid = self.pool.node_id("n_hov");
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });
        if wrap_result {
            self.emit(Op::MakeVariant { name_idx: wrap_idx, arity: 1 });
        }
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // Skip arm: stack already has [v] from the failed Dup; nothing to do.
        let skip_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] {
            *off = skip_target - (j_skip as i32 + 1);
        }

        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    /// Sibling of `emit_variant_map` for the recovery combinators
    /// `result.or_else` and `option.or_else`. Differences from
    /// `emit_variant_map`:
    ///   - matches on the *negative* variant (`Err` / `None`)
    ///   - the closure's result becomes the call's result directly,
    ///     with no wrapping (it is itself a `Result` / `Option`)
    ///   - `option.or_else`'s closure takes zero args (`None` has no
    ///     payload to forward)
    fn emit_variant_or_else(
        &mut self,
        args: &[a::CExpr],
        match_on: &str,
        closure_arity: u16,
    ) {
        let match_idx = self.pool.variant(match_on);

        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__hoe");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__hoe_f");
        self.emit(Op::StoreLocal(f_slot));

        // Stack discipline mirrors emit_variant_map:
        //   load val      ⇒ [v]
        //   dup           ⇒ [v, v]
        //   test          ⇒ [v, Bool]
        //   jumpifnot     ⇒ [v]
        // The unmatched arm leaves [v] (Ok/Some unchanged); the
        // matched arm pops [v] and pushes the closure's result.
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(match_idx));
        let j_skip = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Matched arm: pop the duplicate left on the stack,
        // then call the closure with whatever payload it expects.
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(f_slot));
        if closure_arity == 1 {
            self.emit(Op::LoadLocal(val_slot));
            self.emit(Op::GetVariantArg(0));
        }
        let nid = self.pool.node_id("n_hoe");
        self.emit(Op::CallClosure { arity: closure_arity, node_id_idx: nid });

        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // Unmatched arm: stack already holds [v]; nothing to do.
        let skip_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_skip] {
            *off = skip_target - (j_skip as i32 + 1);
        }

        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    /// `option.unwrap_or_else(opt, f)` — lazy default via zero-arg thunk.
    ///   Some(x) → x          (unwrap; no wrapping)
    ///   None    → f()        (call thunk; return its result directly)
    fn emit_option_unwrap_or_else(&mut self, args: &[a::CExpr]) {
        let some_idx = self.pool.variant("Some");

        // Compile opt and f; stash both so they're accessible on both arms.
        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__uoe_val");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__uoe_f");
        self.emit(Op::StoreLocal(f_slot));

        // Test whether opt is Some.
        //   load val ⇒ [v]
        //   dup      ⇒ [v, v]
        //   test     ⇒ [v, Bool]
        //   jumpifnot → None arm
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(some_idx));
        let j_none = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Some arm: extract the payload from [v] left on the stack.
        self.emit(Op::GetVariantArg(0));
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // None arm: pop the [v] duplicate, call the thunk.
        let none_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_none] {
            *off = none_target - (j_none as i32 + 1);
        }
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(f_slot));
        let nid = self.pool.node_id("n_uoe");
        self.emit(Op::CallClosure { arity: 0, node_id_idx: nid });

        // Patch jump-to-end from Some arm.
        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    /// `result.unwrap_or_else(res, f)` — lazy fallback over the Err payload.
    ///   Ok(x)  → x        (unwrap; no wrapping)
    ///   Err(e) → f(e)     (call closure with the error; result returned directly)
    /// Sibling of `emit_option_unwrap_or_else`; differs only in matching on
    /// `Ok` and forwarding the `Err` payload to a one-arg closure. (#679)
    fn emit_result_unwrap_or_else(&mut self, args: &[a::CExpr]) {
        let ok_idx = self.pool.variant("Ok");

        self.compile_expr(&args[0], false);
        let val_slot = self.alloc_local("__ruoe_val");
        self.emit(Op::StoreLocal(val_slot));

        self.compile_expr(&args[1], false);
        let f_slot = self.alloc_local("__ruoe_f");
        self.emit(Op::StoreLocal(f_slot));

        // Test whether res is Ok.
        //   load val ⇒ [v]
        //   dup      ⇒ [v, v]
        //   test     ⇒ [v, Bool]
        //   jumpifnot → Err arm
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::Dup);
        self.emit(Op::TestVariant(ok_idx));
        let j_err = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // Ok arm: extract the payload from [v] left on the stack.
        self.emit(Op::GetVariantArg(0));
        let j_end = self.code.len();
        self.emit(Op::Jump(0));

        // Err arm: pop the [v] duplicate, call f with the Err payload.
        let err_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_err] {
            *off = err_target - (j_err as i32 + 1);
        }
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(f_slot));
        self.emit(Op::LoadLocal(val_slot));
        self.emit(Op::GetVariantArg(0));
        let nid = self.pool.node_id("n_ruoe");
        self.emit(Op::CallClosure { arity: 1, node_id_idx: nid });

        // Patch jump-to-end from Ok arm.
        let end_target = self.code.len() as i32;
        if let Op::Jump(off) = &mut self.code[j_end] {
            *off = end_target - (j_end as i32 + 1);
        }
    }

    // ---- std.flow trampolines ----------------------------------------
    //
    // Each flow.<op>(c1, c2, ...) call site:
    //   1. compiles its closure args and leaves them on the stack
    //   2. registers a fresh "trampoline" Function whose body invokes
    //      those captured closures appropriately
    //   3. emits MakeClosure { fn_id: trampoline, capture_count: N }
    //
    // The trampoline's parameter layout is [capture_0, ..., capture_{N-1},
    // arg_0, ...]: captures first, the closure's own args after.

    /// Allocate a fresh fn_id for a trampoline and install its bytecode.
    /// Trampolines are the one Function-creation path that already has
    /// the body in hand at install time (top-level fns and lambdas have
    /// it filled in later), so we compute `body_hash` immediately. The
    /// final hash pass at the end of `compile_program` is a no-op here.
    fn install_trampoline(&mut self, name: &str, arity: u16, locals_count: u16, code: Vec<Op>) -> u32 {
        let fn_id = self.next_fn_id.len() as u32;
        let body_hash = crate::program::compute_body_hash(
            arity, locals_count, &code, &self.pool.record_shapes);
        self.next_fn_id.push(Function {
            name: name.into(),
            arity,
            locals_count,
            code,
            effects: Vec::new(),
            body_hash,
            // Trampolines (flow.sequential / parallel / etc.) don't
            // surface refined params at this layer.
            refinements: Vec::new(),
            // Trampolines never emit `Op::GetField` — they're pure
            // scaffolding. Leaving this at 0 means the VM allocates
            // an empty IC slot.
            field_ic_sites: 0,
        });
        fn_id
    }

    /// `flow.sequential(f, g)` returns a closure `(x) -> g(f(x))`.
    fn emit_flow_sequential(&mut self, args: &[a::CExpr]) {
        // Push f, g; build the trampoline closure with 2 captures.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_flow_sequential");
        let code = vec![
            // Locals: [f=0, g=1, x=2]
            Op::LoadLocal(0),                                  // push f
            Op::LoadLocal(2),                                  // push x
            Op::CallClosure { arity: 1, node_id_idx: nid },    // r = f(x)
            // stack: [r]
            Op::StoreLocal(3),                                 // tmp = r
            Op::LoadLocal(1),                                  // push g
            Op::LoadLocal(3),                                  // push tmp
            Op::CallClosure { arity: 1, node_id_idx: nid },    // r = g(tmp)
            Op::Return,
        ];
        let fn_id = self.install_trampoline("__flow_sequential", 3, 4, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.parallel(fa, fb)` returns a closure `() -> (fa(), fb())`.
    /// Implementation is sequential: each function is called in order
    /// and the results are packed into a 2-tuple. The spec (§11.2)
    /// allows the runtime to apply true parallelism here; that needs
    /// a thread-safe handler split and is left to a follow-up. The
    /// signature is what users program against — sequential vs threaded
    /// is an implementation detail invisible to the type system.
    fn emit_flow_parallel(&mut self, args: &[a::CExpr]) {
        // Push fa, fb; build a 0-arg trampoline closure with 2 captures.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let nid = self.pool.node_id("n_flow_parallel");
        let code = vec![
            // Locals: [fa=0, fb=1]
            Op::LoadLocal(0),                                  // push fa
            Op::CallClosure { arity: 0, node_id_idx: nid },    // a = fa()
            Op::LoadLocal(1),                                  // push fb
            Op::CallClosure { arity: 0, node_id_idx: nid },    // b = fb()
            Op::MakeTuple(2),                                  // (a, b)
            Op::Return,
        ];
        let fn_id = self.install_trampoline("__flow_parallel", 2, 2, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.parallel_list(actions)` runs each 0-arg closure in `actions`
    /// and returns the results as a list in input order. Variadic
    /// counterpart to `flow.parallel`. Sequential under the hood — the
    /// spec (§11.2) reserves true threading for a future scheduler.
    /// Compiled inline (mirrors `list.map`) so closure args can flow
    /// through `CallClosure` without a heap-allocated trampoline.
    fn emit_flow_parallel_list(&mut self, args: &[a::CExpr]) {
        // xs := actions
        self.compile_expr(&args[0], false);
        let xs = self.alloc_local("__fpl_xs");
        self.emit(Op::StoreLocal(xs));

        // out := []
        self.emit(Op::MakeList(0));
        let out = self.alloc_local("__fpl_out");
        self.emit(Op::StoreLocal(out));

        // i := 0
        let zero = self.pool.int(0);
        self.emit(Op::PushConst(zero));
        let i = self.alloc_local("__fpl_i");
        self.emit(Op::StoreLocal(i));

        // loop_top: while i < len(xs) { ... }
        let loop_top = self.code.len();
        self.emit(Op::LoadLocal(i));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::GetListLen);
        self.emit(Op::IntLt);
        let j_exit = self.code.len();
        self.emit(Op::JumpIfNot(0));

        // body: out := out ++ [xs[i]()]
        let nid = self.pool.node_id("n_flow_parallel_list");
        self.emit(Op::LoadLocal(out));
        self.emit(Op::LoadLocal(xs));
        self.emit(Op::LoadLocal(i));
        self.emit(Op::GetListElemDyn);
        self.emit(Op::CallClosure { arity: 0, node_id_idx: nid });
        self.emit(Op::ListAppend);
        self.emit(Op::StoreLocal(out));

        // i := i + 1
        self.emit(Op::LoadLocal(i));
        let one = self.pool.int(1);
        self.emit(Op::PushConst(one));
        self.emit(Op::IntAdd);
        self.emit(Op::StoreLocal(i));

        // jump back
        let jump_back = self.code.len();
        let back = (loop_top as i32) - (jump_back as i32 + 1);
        self.emit(Op::Jump(back));

        // exit: patch j_exit, push out
        let exit_target = self.code.len() as i32;
        if let Op::JumpIfNot(off) = &mut self.code[j_exit] {
            *off = exit_target - (j_exit as i32 + 1);
        }
        self.emit(Op::LoadLocal(out));
    }

    /// `flow.branch(cond, t, f)` returns a closure `(x) -> if cond(x) then t(x) else f(x)`.
    fn emit_flow_branch(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        self.compile_expr(&args[2], false);
        let nid = self.pool.node_id("n_flow_branch");
        let mut code = vec![
            // Locals: [cond=0, t=1, f=2, x=3]
            Op::LoadLocal(0),                               // push cond
            Op::LoadLocal(3),                               // push x
            Op::CallClosure { arity: 1, node_id_idx: nid }, // bool
        ];
        let j_false = code.len();
        code.push(Op::JumpIfNot(0));                        // patched
        // true arm: t(x)
        code.push(Op::LoadLocal(1));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: nid });
        code.push(Op::Return);
        // false arm
        let false_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_false] {
            *off = false_target - (j_false as i32 + 1);
        }
        code.push(Op::LoadLocal(2));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: nid });
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_branch", 4, 4, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 3 });
    }

    /// `flow.retry(f, max_attempts)` returns a closure `(x) -> Result[U, E]`
    /// that calls `f(x)` up to `max_attempts` times, returning the first
    /// `Ok` or the final `Err`.
    fn emit_flow_retry(&mut self, args: &[a::CExpr]) {
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        let call_nid = self.pool.node_id("n_flow_retry");
        let ok_idx = self.pool.variant("Ok");
        let zero_const = self.pool.int(0);
        let one_const = self.pool.int(1);
        // Locals: [f=0, max=1, x=2, i=3, last=4]
        let mut code = vec![
            // i := 0
            Op::PushConst(zero_const),
            Op::StoreLocal(3),
        ];
        // loop_top: while i < max
        let loop_top = code.len() as i32;
        code.push(Op::LoadLocal(3));
        code.push(Op::LoadLocal(1));
        code.push(Op::IntLt);
        let j_done = code.len();
        code.push(Op::JumpIfNot(0));                       // patched

        // body: r := f(x); last := r
        code.push(Op::LoadLocal(0));
        code.push(Op::LoadLocal(2));
        code.push(Op::CallClosure { arity: 1, node_id_idx: call_nid });
        code.push(Op::StoreLocal(4));

        // Test variant Ok on last; if so, return last.
        code.push(Op::LoadLocal(4));
        code.push(Op::TestVariant(ok_idx));
        let j_was_err = code.len();
        code.push(Op::JumpIfNot(0));                       // patched: skip return
        code.push(Op::LoadLocal(4));
        code.push(Op::Return);

        // was_err: i := i + 1; jump loop_top
        let was_err_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_was_err] {
            *off = was_err_target - (j_was_err as i32 + 1);
        }
        code.push(Op::LoadLocal(3));
        code.push(Op::PushConst(one_const));
        code.push(Op::IntAdd);
        code.push(Op::StoreLocal(3));
        let pc_after_jump = code.len() as i32 + 1;
        code.push(Op::Jump(loop_top - pc_after_jump));

        // done: return last (the final Err, or Unit if max=0).
        let done_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_done] {
            *off = done_target - (j_done as i32 + 1);
        }
        code.push(Op::LoadLocal(4));
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_retry", 3, 5, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 2 });
    }

    /// `flow.retry_with_backoff(f, attempts, base_ms)` (#226). Variant
    /// of `flow.retry` that sleeps between attempts. The first
    /// attempt fires immediately; attempt k > 1 waits `base_ms *
    /// 2^(k-2)` ms before retrying. Sleeps go through
    /// `time.sleep_ms`, which is why the resulting closure carries
    /// `[time]` in its effect row even though the underlying `f` is
    /// pure.
    fn emit_flow_retry_with_backoff(&mut self, args: &[a::CExpr]) {
        // Push captures: f, max, base_ms. The trampoline takes one
        // call-time arg `x`, so capture_count = 3, arity = 4.
        self.compile_expr(&args[0], false);
        self.compile_expr(&args[1], false);
        self.compile_expr(&args[2], false);
        let call_nid    = self.pool.node_id("n_flow_retry_backoff");
        let sleep_nid   = self.pool.node_id("n_flow_retry_backoff_sleep");
        let kind_idx    = self.pool.str("time");
        let op_idx      = self.pool.str("sleep_ms");
        let ok_idx      = self.pool.variant("Ok");
        let zero_const  = self.pool.int(0);
        let one_const   = self.pool.int(1);
        let two_const   = self.pool.int(2);
        // Locals layout:
        //   0=f, 1=max, 2=base_ms (captures)
        //   3=x (arg)
        //   4=i, 5=last, 6=next_delay (working state)
        let mut code = vec![
            // next_delay := base_ms
            Op::LoadLocal(2),
            Op::StoreLocal(6),
            // i := 0
            Op::PushConst(zero_const),
            Op::StoreLocal(4),
        ];

        let loop_top = code.len() as i32;
        // while i < max
        code.push(Op::LoadLocal(4));
        code.push(Op::LoadLocal(1));
        code.push(Op::IntLt);
        let j_done = code.len();
        code.push(Op::JumpIfNot(0)); // patched

        // if i > 0: time.sleep_ms(next_delay); next_delay := next_delay * 2
        code.push(Op::PushConst(zero_const));
        code.push(Op::LoadLocal(4));
        code.push(Op::IntLt);                // 0 < i ?
        let j_no_sleep = code.len();
        code.push(Op::JumpIfNot(0));         // patched: skip the sleep block
        // Sleep
        code.push(Op::LoadLocal(6));         // arg = next_delay
        code.push(Op::EffectCall {
            kind_idx, op_idx, arity: 1, node_id_idx: sleep_nid,
        });
        code.push(Op::Pop);                  // discard the Unit result
        // next_delay := next_delay * 2
        code.push(Op::LoadLocal(6));
        code.push(Op::PushConst(two_const));
        code.push(Op::NumMul);
        code.push(Op::StoreLocal(6));
        // patch the no-sleep skip
        let after_sleep = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_no_sleep] {
            *off = after_sleep - (j_no_sleep as i32 + 1);
        }

        // last := f(x)
        code.push(Op::LoadLocal(0));
        code.push(Op::LoadLocal(3));
        code.push(Op::CallClosure { arity: 1, node_id_idx: call_nid });
        code.push(Op::StoreLocal(5));

        // if Ok(last): return last
        code.push(Op::LoadLocal(5));
        code.push(Op::TestVariant(ok_idx));
        let j_was_err = code.len();
        code.push(Op::JumpIfNot(0)); // patched
        code.push(Op::LoadLocal(5));
        code.push(Op::Return);

        // was_err: i := i + 1; jump loop_top
        let was_err_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_was_err] {
            *off = was_err_target - (j_was_err as i32 + 1);
        }
        code.push(Op::LoadLocal(4));
        code.push(Op::PushConst(one_const));
        code.push(Op::IntAdd);
        code.push(Op::StoreLocal(4));
        let pc_after_jump = code.len() as i32 + 1;
        code.push(Op::Jump(loop_top - pc_after_jump));

        // done: return last (the final Err, or Unit if max=0).
        let done_target = code.len() as i32;
        if let Op::JumpIfNot(off) = &mut code[j_done] {
            *off = done_target - (j_done as i32 + 1);
        }
        code.push(Op::LoadLocal(5));
        code.push(Op::Return);

        let fn_id = self.install_trampoline("__flow_retry_backoff", 4, 7, code);
        self.emit(Op::MakeClosure { fn_id, capture_count: 3 });
    }
}

/// Collect free variables referenced in `e` that are not in `bound`.
/// Mutates `bound` to track let/lambda introductions during the walk;
/// the caller's set is preserved on return because Rust's borrow rules
/// force us to clone for sub-scopes that rebind a name.
fn free_vars(e: &a::CExpr, bound: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
    match e {
        a::CExpr::Literal { .. } => {}
        a::CExpr::Var { name } => {
            if !bound.contains(name) && !out.contains(name) {
                out.push(name.clone());
            }
        }
        a::CExpr::Call { callee, args } => {
            free_vars(callee, bound, out);
            for a in args { free_vars(a, bound, out); }
        }
        a::CExpr::Let { name, value, body, .. } => {
            free_vars(value, bound, out);
            let was_bound = bound.contains(name);
            bound.insert(name.clone());
            free_vars(body, bound, out);
            if !was_bound { bound.remove(name); }
        }
        a::CExpr::Match { scrutinee, arms } => {
            free_vars(scrutinee, bound, out);
            for arm in arms {
                let mut local_bound = bound.clone();
                pattern_binders(&arm.pattern, &mut local_bound);
                free_vars(&arm.body, &mut local_bound, out);
            }
        }
        a::CExpr::Block { statements, result } => {
            let mut local_bound = bound.clone();
            for s in statements { free_vars(s, &mut local_bound, out); }
            free_vars(result, &mut local_bound, out);
        }
        a::CExpr::Constructor { args, .. } => {
            for a in args { free_vars(a, bound, out); }
        }
        a::CExpr::RecordLit { fields } => {
            for f in fields { free_vars(&f.value, bound, out); }
        }
        a::CExpr::TupleLit { items } | a::CExpr::ListLit { items } => {
            for it in items { free_vars(it, bound, out); }
        }
        a::CExpr::FieldAccess { value, .. } => free_vars(value, bound, out),
        a::CExpr::Lambda { params, body, .. } => {
            let mut inner = bound.clone();
            for p in params { inner.insert(p.name.clone()); }
            free_vars(body, &mut inner, out);
        }
        a::CExpr::BinOp { lhs, rhs, .. } => {
            free_vars(lhs, bound, out);
            free_vars(rhs, bound, out);
        }
        a::CExpr::UnaryOp { expr, .. } => free_vars(expr, bound, out),
        a::CExpr::Return { value } => free_vars(value, bound, out),
    }
}

fn pattern_binders(p: &a::Pattern, bound: &mut std::collections::HashSet<String>) {
    match p {
        a::Pattern::PWild | a::Pattern::PLiteral { .. } => {}
        a::Pattern::PVar { name } => { bound.insert(name.clone()); }
        a::Pattern::PConstructor { args, .. } => {
            for a in args { pattern_binders(a, bound); }
        }
        a::Pattern::PRecord { fields } => {
            for f in fields { pattern_binders(&f.pattern, bound); }
        }
        a::Pattern::PTuple { items } => {
            for it in items { pattern_binders(it, bound); }
        }
    }
}
