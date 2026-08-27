//! `lex test` — run test_*.lex files.
//!
//! Convention: each test file exports `run_all`. The runner compiles the file,
//! calls it, and reports pass/fail per file. Exit 0 iff every file passes.
//!
//! A file fails if `run_all` raises, OR if it *returns* a failure — a non-zero
//! `Int` count, or a `List` containing any `Err`. Honouring the return value is
//! #757: discarding it silently passed suites that reported failures rather
//! than aborting, which is most of them, because `std.test.assert_*` returns a
//! `Result` instead of raising.
//!
//! Flags:
//!   --allow-effects k1,k2,...  override the runner's permissive
//!                              policy with an explicit allow-list.
//!                              Useful for tests that touch vendor-
//!                              extension effects not in the stdlib
//!                              catalog (#399).

use anyhow::Result;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_syntax::load_program;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn cmd_test(_fmt: &::acli::OutputFormat, args: &[String]) -> Result<()> {
    let mut dir: Option<PathBuf> = None;
    let mut explicit_effects: Option<BTreeSet<String>> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-effects" => {
                let val = args.get(i + 1).ok_or_else(|| {
                    anyhow::anyhow!("--allow-effects requires a value")
                })?;
                explicit_effects = Some(
                    val.split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect(),
                );
                i += 2;
            }
            other if !other.starts_with('-') && dir.is_none() => {
                dir = Some(PathBuf::from(other));
                i += 1;
            }
            other => anyhow::bail!(
                "unknown flag `{other}`; usage: lex test [--allow-effects k1,k2,...] [<dir>]"
            ),
        }
    }
    let dir = dir.unwrap_or_else(|| PathBuf::from("tests"));

    let entries = collect_test_files(&dir)?;
    if entries.is_empty() {
        println!("no test_*.lex files found in {}", dir.display());
        return Ok(());
    }

    let mut pass = 0usize;
    let mut fail = 0usize;

    for path in &entries {
        match run_one(path, explicit_effects.as_ref()) {
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

fn run_one(path: &Path, explicit_effects: Option<&BTreeSet<String>>) -> Result<()> {
    // Route through the multi-file loader so `import "./foo" as f`,
    // `import "../src/lib" as lib`, and `import "pkg-name/mod" as p`
    // resolve and get mangled the same way `lex check` does. Without
    // this, every aliased import in a test file blows up with
    // `unknown_identifier` at type-check time (#395).
    let prog = load_program(path)
        .map_err(|e| anyhow::anyhow!("load: {e}"))?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let msgs: Vec<String> = errs
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| format!("{e:?}")))
            .collect();
        anyhow::bail!("type error: {}", msgs.join("; "));
    }
    let bc = compile_program(&stages);
    // Permissive by default — covers every effect declared in the
    // stdlib catalog (#399). `--allow-effects k1,k2,...` is the escape
    // hatch for vendor-extension effects we don't know about, and also
    // for restricting a test to a tight allow-list when verifying
    // effect-shape contracts.
    let policy = match explicit_effects {
        Some(set) => {
            let mut p = Policy::pure();
            p.allow_effects = set.clone();
            p
        }
        None => Policy::permissive(),
    };
    check_policy(&bc, &policy).map_err(|v| anyhow::anyhow!("policy: {v:?}"))?;
    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let returned = vm
        .call("run_all", vec![])
        .map_err(|e| anyhow::anyhow!("runtime: {e}"))?;
    interpret_result(returned)
}

/// Decide pass/fail from what `run_all` returned.
///
/// The documented convention is `fn run_all() -> ()` asserting or panicking, and
/// for a long time the return value was simply discarded. That silently passed
/// every file whose `run_all` *reported* failures instead of raising — which is
/// the shape most suites in the wild actually use, since `std.test.assert_*`
/// returns a `Result` rather than aborting. A green run then meant "the file
/// loaded", not "the assertions held" (#757).
///
/// So the value is now honoured, in the three shapes suites actually return:
///
///   `Unit`                     — the documented convention; the file raised or
///                                it passed
///   `Int`                      — a failure count; non-zero fails
///   `List[Result[_, _]]`       — the results themselves; any `Err` fails
///
/// Anything else passes, so an unusual-but-deliberate `run_all` is not broken by
/// this. The point is to stop *silently* passing a file that said it failed.
fn interpret_result(v: lex_bytecode::value::Value) -> Result<()> {
    use lex_bytecode::value::Value;
    match v {
        Value::Int(0) | Value::Unit => Ok(()),
        Value::Int(n) => anyhow::bail!(
            "run_all reported {n} failing assertion(s) — run it directly to see which"
        ),
        Value::List(items) => {
            let failed: Vec<String> = items
                .iter()
                .filter_map(|it| match it {
                    Value::Variant { name, args } if name == "Err" => Some(
                        args.first()
                            .map(|a| match a {
                                Value::Str(s) => s.to_string(),
                                other => format!("{other:?}"),
                            })
                            .unwrap_or_else(|| "unspecified".into()),
                    ),
                    _ => None,
                })
                .collect();
            if failed.is_empty() {
                Ok(())
            } else {
                anyhow::bail!("{} assertion(s) failed: {}", failed.len(), failed.join("; "))
            }
        }
        _ => Ok(()),
    }
}
