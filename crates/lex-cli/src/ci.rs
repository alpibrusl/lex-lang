//! `lex ci` — run the full CI pipeline locally.
//!
//! Equivalent of what `.github/workflows/lex.yml` does:
//!   1. lex pkg install           install / verify all declared dependencies
//!   2. lex check --strict <src>  type-check with lint warnings
//!   3. lex fmt --check <src>     verify formatting
//!   4. lex test                  run the test suite
//!
//! Usage:
//!   lex ci [--no-fmt] [--src <dir>] [--tests <dir>]
//!
//! Defaults: src=src/, tests=tests/. Pass --no-fmt to skip the format check
//! (useful while actively editing).
//!
//! Exits 0 only if every step passes.

use anyhow::Result;
use std::path::Path;

pub fn cmd_ci(args: &[String]) -> Result<()> {
    let mut no_fmt    = false;
    let mut src_dir   = "src".to_string();
    let mut tests_dir = "tests".to_string();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--no-fmt" => no_fmt = true,
            "--src" => {
                i += 1;
                src_dir = args.get(i).cloned()
                    .ok_or_else(|| anyhow::anyhow!("--src requires a value"))?;
            }
            "--tests" => {
                i += 1;
                tests_dir = args.get(i).cloned()
                    .ok_or_else(|| anyhow::anyhow!("--tests requires a value"))?;
            }
            other => anyhow::bail!("unknown flag `{other}`; usage: lex ci [--no-fmt] [--src <dir>] [--tests <dir>]"),
        }
        i += 1;
    }

    let mut failures: Vec<&'static str> = Vec::new();

    // ── Step 1: pkg install ───────────────────────────────────────────────────
    println!("==> lex pkg install");
    if let Err(e) = crate::pkg::cmd_pkg(&["install".to_string()]) {
        eprintln!("  FAILED: {e:#}");
        failures.push("pkg install");
    }

    // ── Step 2: check --strict ────────────────────────────────────────────────
    println!("\n==> lex check --strict {src_dir}/");
    let src_files = collect_lex_files(&src_dir);
    if src_files.is_empty() {
        println!("  no .lex files in {src_dir}/");
    } else {
        for file in &src_files {
            print!("  checking {} ... ", file.display());
            let check_args: Vec<String> = vec![
                "--strict".to_string(),
                file.display().to_string(),
            ];
            match run_check(&check_args) {
                Ok(()) => println!("ok"),
                Err(e) => {
                    println!("FAILED");
                    eprintln!("  {e:#}");
                    failures.push("check");
                }
            }
        }
    }

    // ── Step 3: fmt --check ───────────────────────────────────────────────────
    if !no_fmt {
        let dirs_to_fmt: Vec<String> = [src_dir.as_str(), tests_dir.as_str()]
            .iter()
            .filter(|d| Path::new(d).is_dir())
            .map(|d| d.to_string())
            .collect();

        if !dirs_to_fmt.is_empty() {
            println!("\n==> lex fmt --check {}", dirs_to_fmt.join(" "));
            let mut fmt_args = vec!["--check".to_string()];
            fmt_args.extend(dirs_to_fmt);
            if let Err(e) = crate::fmt::cmd_fmt(&fmt_args) {
                eprintln!("  FAILED: {e:#}");
                failures.push("fmt --check");
            }
        }
    }

    // ── Step 4: test ─────────────────────────────────────────────────────────
    println!("\n==> lex test");
    let fmt = ::acli::OutputFormat::Text;
    if let Err(e) = crate::test_runner::cmd_test(&fmt, &[]) {
        eprintln!("  FAILED: {e:#}");
        failures.push("test");
    }

    // ── Summary ───────────────────────────────────────────────────────────────
    println!();
    if failures.is_empty() {
        println!("CI passed — all steps green");
        Ok(())
    } else {
        anyhow::bail!("CI failed: {}", failures.join(", "))
    }
}

fn collect_lex_files(dir: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    collect_recursive(Path::new(dir), &mut out);
    out.sort();
    out
}

fn collect_recursive(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let p = entry.path();
        if p.is_dir() {
            collect_recursive(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("lex") {
            out.push(p);
        }
    }
}

fn run_check(args: &[String]) -> Result<()> {
    // Delegate to the check function in main by re-entrant call via
    // the same process. We call cmd_check directly since it's in the
    // same binary (not pub, so we route through main's run()).
    //
    // Simpler approach: shell out to ourselves.
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from("lex"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("check");
    for a in args { cmd.arg(a); }
    let status = cmd.status()
        .map_err(|e| anyhow::anyhow!("could not run `lex check`: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("`lex check` exited with {}", status))
    }
}
