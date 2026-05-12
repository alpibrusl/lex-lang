//! `lex test` — run test_*.lex files.
//!
//! Convention: each test file exports `fn run_all() -> ()`. The runner
//! compiles the file, calls `run_all`, and reports pass/fail per file.
//! Exit 0 iff every file passes.

use anyhow::Result;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::path::{Path, PathBuf};

pub fn cmd_test(_fmt: &::acli::OutputFormat, args: &[String]) -> Result<()> {
    let dir: PathBuf = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tests"));

    let entries = collect_test_files(&dir)?;
    if entries.is_empty() {
        println!("no test_*.lex files found in {}", dir.display());
        return Ok(());
    }

    let mut pass = 0usize;
    let mut fail = 0usize;

    for path in &entries {
        match run_one(path) {
            Ok(()) => {
                println!("ok   {}", path.display());
                pass += 1;
            }
            Err(e) => {
                println!("FAIL {} — {e:#}", path.display());
                fail += 1;
            }
        }
    }

    println!("\n{} passed, {} failed", pass, fail);
    if fail > 0 {
        anyhow::bail!("{fail} test file(s) failed");
    }
    Ok(())
}

fn collect_test_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("lex") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("test_") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn run_one(path: &Path) -> Result<()> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read: {e}"))?;
    let prog = parse_source(&src)
        .map_err(|e| anyhow::anyhow!("parse: {e}"))?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let msgs: Vec<String> = errs
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| format!("{e:?}")))
            .collect();
        anyhow::bail!("type error: {}", msgs.join("; "));
    }
    let bc = compile_program(&stages);
    let policy = Policy::permissive();
    check_policy(&bc, &policy).map_err(|v| anyhow::anyhow!("policy: {v:?}"))?;
    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call("run_all", vec![])
        .map_err(|e| anyhow::anyhow!("runtime: {e}"))?;
    Ok(())
}
