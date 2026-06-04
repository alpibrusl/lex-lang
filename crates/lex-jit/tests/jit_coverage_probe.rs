//! Eligibility coverage probe: scans real Lex programs (the
//! repo's `examples/` tree) and reports what fraction of compiled
//! functions the MVP JIT's `is_jit_eligible` predicate accepts.
//!
//! Run with:
//!
//!   cargo test -p lex-jit --features cranelift \
//!       --test jit_coverage_probe -- --nocapture --test-threads=1
//!
//! Output is per-file plus a workspace-wide rollup, including the
//! single most common rejection reason per file. Marked `#[ignore]`
//! so `cargo test` doesn't run it by default — it's a diagnostic
//! probe, not a regression check.

#![cfg(feature = "cranelift")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lex_ast::canonicalize_program;
use lex_bytecode::compile_program;
use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::{Function, Program};
use lex_jit::is_jit_eligible;
use lex_syntax::parse_source;

/// Categorize the *first* disqualifying op in a function. The
/// eligibility predicate short-circuits on the first reject, so
/// this matches what the JIT would actually complain about.
fn first_disqualifier(f: &Function, consts: &[Const]) -> String {
    if f.arity > 6 {
        return "arity>6".into();
    }
    for op in &f.code {
        match op {
            Op::PushConst(i) => match consts.get(*i as usize) {
                Some(Const::Int(_)) | Some(Const::Bool(_)) => {}
                Some(Const::Str(_)) => return "PushConst(Str)".into(),
                Some(Const::Float(_)) => return "PushConst(Float)".into(),
                Some(Const::FieldName(_)) => return "PushConst(FieldName)".into(),
                Some(Const::VariantName(_)) => return "PushConst(VariantName)".into(),
                Some(_) => return "PushConst(other)".into(),
                None => return "PushConst(missing)".into(),
            },
            Op::Pop
            | Op::LoadLocal(_)
            | Op::StoreLocal(_)
            | Op::IntAdd
            | Op::IntSub
            | Op::IntMul
            | Op::IntDiv
            | Op::IntMod
            | Op::IntNeg
            | Op::IntEq
            | Op::IntLt
            | Op::IntLe
            | Op::BoolAnd
            | Op::BoolOr
            | Op::BoolNot
            | Op::Jump(_)
            | Op::JumpIf(_)
            | Op::JumpIfNot(_)
            | Op::Return => {}
            // Map the long Op enum to short category labels — the
            // raw variant Debug names are too noisy to bucket on.
            Op::Call { .. } | Op::TailCall { .. } | Op::CallClosure { .. } => return "Call*".into(),
            Op::MakeRecord { .. }
            | Op::AllocStackRecord { .. }
            | Op::AllocArenaRecord { .. }
            | Op::GetField { .. }
            | Op::LoadLocalGetField { .. }
            | Op::LoadLocalGetFieldAdd { .. }
            | Op::LoadLocalGetFieldSub { .. }
            | Op::LoadLocalGetFieldMul { .. } => return "Record".into(),
            Op::MakeTuple(_)
            | Op::AllocStackTuple { .. }
            | Op::AllocArenaTuple { .. }
            | Op::GetElem(_) => return "Tuple".into(),
            Op::MakeList(_)
            | Op::GetListLen
            | Op::GetListElem(_)
            | Op::GetListElemDyn
            | Op::ListAppend
            | Op::ListMap { .. }
            | Op::ListFilter { .. }
            | Op::ListFold { .. }
            | Op::SortByKey { .. }
            | Op::ParallelMap { .. } => return "List".into(),
            Op::MakeVariant { .. } | Op::TestVariant(_) | Op::GetVariant(_) | Op::GetVariantArg(_) => {
                return "Variant".into()
            }
            Op::MakeClosure { .. } => return "Closure".into(),
            Op::FloatAdd
            | Op::FloatSub
            | Op::FloatMul
            | Op::FloatDiv
            | Op::FloatNeg
            | Op::FloatEq
            | Op::FloatLt
            | Op::FloatLe => return "Float".into(),
            Op::NumAdd | Op::NumSub | Op::NumMul | Op::NumDiv | Op::NumMod | Op::NumNeg
            | Op::NumEq | Op::NumLt | Op::NumLe => return "Num*".into(),
            Op::StrConcat | Op::StrLen | Op::StrEq | Op::BytesLen | Op::BytesEq => {
                return "Str/Bytes".into()
            }
            Op::EffectCall { .. } => return "EffectCall".into(),
            Op::Panic(_) => return "Panic".into(),
            Op::Dup => return "Dup".into(),
            // Catch-all for superinstructions and anything else.
            _ => return "Superinstruction/Other".into(),
        }
    }
    "no-return-on-reachable-path".into()
}

fn compile_file(path: &Path) -> Option<Program> {
    let src = std::fs::read_to_string(path).ok()?;
    let prog = parse_source(&src).ok()?;
    let stages = canonicalize_program(&prog);
    // Some example files fail type-checking (intentionally broken
    // fuzz corpora, or older Lex syntax). Skip silently — they're
    // not signal for the JIT-coverage question anyway.
    lex_types::check_program(&stages).ok()?;
    Some(compile_program(&stages))
}

fn lex_files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("lex") {
            out.push(path);
        }
    }
}

#[test]
#[ignore = "diagnostic probe — run explicitly with --nocapture"]
fn jit_eligibility_coverage_report() {
    // Run from the crates/lex-jit/ working dir, so escape to repo root.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root")
        .to_path_buf();

    // Only probe the canonical `examples/` tree. The fuzz corpora
    // and stdlib are interesting but noisy; start narrow.
    let scan_dir = repo_root.join("examples");
    let files = lex_files_under(&scan_dir);

    println!("\n=== JIT eligibility coverage — examples/ ===");
    println!("scanning: {}", scan_dir.display());
    println!("files found: {}\n", files.len());

    let mut total_fns = 0usize;
    let mut total_eligible = 0usize;
    let mut compile_failures = 0usize;
    let mut reason_totals: BTreeMap<String, usize> = BTreeMap::new();

    for file in &files {
        let Some(prog) = compile_file(file) else {
            compile_failures += 1;
            continue;
        };
        let mut n = 0;
        let mut elig = 0;
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        for (fid, f) in prog.functions.iter().enumerate() {
            n += 1;
            if is_jit_eligible(fid as u32, f, &prog.constants) {
                elig += 1;
            } else {
                let r = first_disqualifier(f, &prog.constants);
                *reasons.entry(r.clone()).or_default() += 1;
                *reason_totals.entry(r).or_default() += 1;
            }
        }
        total_fns += n;
        total_eligible += elig;

        let rel = file.strip_prefix(&repo_root).unwrap_or(file).display();
        let pct = if n == 0 { 0.0 } else { 100.0 * elig as f64 / n as f64 };
        println!(
            "{:>3}/{:<3} ({:5.1}%) eligible — {}",
            elig, n, pct, rel
        );
        if !reasons.is_empty() && elig < n {
            let mut top: Vec<_> = reasons.iter().collect();
            top.sort_by_key(|&(_, c)| std::cmp::Reverse(*c));
            let leaders: Vec<String> = top
                .iter()
                .take(3)
                .map(|(k, v)| format!("{k}:{v}"))
                .collect();
            println!("             top reject reasons: {}", leaders.join(", "));
        }
    }

    println!("\n--- rollup ---");
    println!("files compiled: {}", files.len() - compile_failures);
    println!("files skipped (parse/typecheck failed): {compile_failures}");
    println!(
        "functions: {total_eligible}/{total_fns} eligible ({:.1}%)",
        if total_fns == 0 { 0.0 } else { 100.0 * total_eligible as f64 / total_fns as f64 }
    );
    println!("\ntop disqualifying ops across all functions:");
    let mut all: Vec<_> = reason_totals.iter().collect();
    all.sort_by_key(|&(_, c)| std::cmp::Reverse(*c));
    for (reason, count) in all {
        println!("  {:>4}  {reason}", count);
    }
}
