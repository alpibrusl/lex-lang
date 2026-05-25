//! `lex fmt` — format Lex source files using the canonical pretty-printer.
//!
//! Usage:
//!   lex fmt [--check] <file|dir>...
//!
//! Without --check: rewrites each file in-place and prints the list of
//! files that were changed.
//!
//! With --check: prints files that would change and exits 1 if any
//! would (suitable for CI).
//!
//! Directories are expanded to all *.lex files found recursively.

use anyhow::{Context, Result};
use lex_syntax::{parse_source, print_program};
use std::path::{Path, PathBuf};

pub fn cmd_fmt(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut paths: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            p => paths.push(PathBuf::from(p)),
        }
        i += 1;
    }

    if paths.is_empty() {
        anyhow::bail!("usage: lex fmt [--check] <file|dir>...");
    }

    let files = collect_lex_files(&paths)?;
    if files.is_empty() {
        println!("no .lex files found");
        return Ok(());
    }

    let mut changed = Vec::new();
    let mut errors  = Vec::new();

    for file in &files {
        match fmt_file(file) {
            Ok(Some(formatted)) => {
                if check {
                    changed.push(file.clone());
                } else {
                    std::fs::write(file, &formatted)
                        .with_context(|| format!("writing {}", file.display()))?;
                    changed.push(file.clone());
                }
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("{}: {e}", file.display())),
        }
    }

    for e in &errors {
        eprintln!("error: {e}");
    }

    if check {
        for f in &changed {
            println!("would reformat {}", f.display());
        }
        if !changed.is_empty() {
            anyhow::bail!("{} file(s) need formatting (run `lex fmt` to fix)", changed.len());
        }
        println!("all {} file(s) are formatted", files.len());
    } else {
        for f in &changed {
            println!("reformatted {}", f.display());
        }
        let unchanged = files.len() - changed.len();
        if unchanged > 0 || !changed.is_empty() {
            println!(
                "{} reformatted, {} unchanged",
                changed.len(),
                unchanged
            );
        }
    }

    if !errors.is_empty() {
        anyhow::bail!("{} file(s) had parse errors", errors.len());
    }
    Ok(())
}

/// Returns `Some(formatted)` if the file needs reformatting, `None` if already canonical.
fn fmt_file(path: &Path) -> Result<Option<String>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let prog = parse_source(&src)
        .map_err(|e| anyhow::anyhow!("parse error: {e:?}"))?;
    let formatted = print_program(&prog);
    if formatted == src {
        Ok(None)
    } else {
        Ok(Some(formatted))
    }
}

/// Expand a list of file/directory paths to a sorted list of .lex files.
pub(crate) fn collect_lex_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_recursive(path, &mut files);
        } else if path.extension().and_then(|e| e.to_str()) == Some("lex") {
            files.push(path.clone());
        } else {
            anyhow::bail!("{} is not a .lex file or directory", path.display());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
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
