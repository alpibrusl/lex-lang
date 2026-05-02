//! `lex watch <file> [check|run] [args...]` — re-run a command on
//! every save of `<file>`. The agent inner loop: edit → save →
//! see results in <100ms, no terminal switching.
//!
//! Default action is `check`. `lex watch app.lex run main` re-runs
//! the program on save. Forwarded args (`--allow-effects ...`)
//! pass through to the underlying subcommand.
//!
//! v1 watches a single file; multi-file projects can pass the
//! directory and we'll re-run on any `.lex` save under it.

use anyhow::{anyhow, Context, Result};
use notify::{event::EventKind, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

pub fn cmd_watch(args: &[String]) -> Result<()> {
    let path = args.first()
        .ok_or_else(|| anyhow!("usage: lex watch <file> [check|run] [forwarded args...]"))?;
    let action = args.get(1).cloned().unwrap_or_else(|| "check".into());
    let forwarded: Vec<String> = if args.len() > 2 { args[2..].to_vec() } else { Vec::new() };

    if !matches!(action.as_str(), "check" | "run") {
        return Err(anyhow!(
            "watch action must be `check` or `run`, got `{action}`. \
             usage: lex watch <file> [check|run] [args...]"));
    }

    let watch_path = PathBuf::from(path);
    if !watch_path.exists() {
        return Err(anyhow!("watch target does not exist: {path}"));
    }

    eprintln!("→ watching {} (action: {action})", watch_path.display());

    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }).context("create file watcher")?;
    let mode = if watch_path.is_dir() { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    watcher.watch(&watch_path, mode).context("start watching")?;

    // Run once on startup so the user sees baseline output.
    run_once(&action, path, &forwarded);

    let mut last_run = Instant::now();
    let debounce = Duration::from_millis(150);

    for event in rx {
        let event = match event {
            Ok(e) => e, Err(e) => { eprintln!("watch: {e}"); continue; }
        };
        // Filter: only react to writes (saves), creates, renames.
        // Many editors do atomic-rename-on-save, so all three matter.
        let actionable = matches!(event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_));
        if !actionable { continue; }
        // Only react to .lex files when watching a directory.
        if mode == RecursiveMode::Recursive
            && !event.paths.iter().any(|p| is_lex_file(p))
        { continue; }
        // Debounce: editors often emit multiple events per save.
        if last_run.elapsed() < debounce { continue; }
        last_run = Instant::now();

        run_once(&action, path, &forwarded);
    }
    Ok(())
}

fn run_once(action: &str, path: &str, forwarded: &[String]) {
    println!("\n────────────────────────────────────────────────────────");
    println!("⟲ {action} {path}{}",
        if forwarded.is_empty() { String::new() } else { format!(" {}", forwarded.join(" ")) });
    println!("────────────────────────────────────────────────────────");
    let started = Instant::now();
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("lex"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg(action).arg(path);
    for a in forwarded { cmd.arg(a); }
    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => { eprintln!("watch: spawn `{}`: {e}", exe.display()); return; }
    };
    let elapsed = started.elapsed();
    let code = status.code().unwrap_or(-1);
    let icon = if status.success() { "✓" } else { "✗" };
    println!("{icon} exit {code}  ({:.0}ms)", elapsed.as_secs_f64() * 1000.0);
}

fn is_lex_file(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()) == Some("lex")
}
