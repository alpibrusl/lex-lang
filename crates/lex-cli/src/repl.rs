//! `lex repl` — interactive evaluator.
//!
//! v1 design: REPL state is the *Lex source* of all stages defined
//! since startup, kept in a `String`. On every input, we splice the
//! input into the program (either as a new top-level stage or as an
//! expression wrapped in `fn __repl_eval() -> _ { … }`) and re-parse
//! / re-type-check / re-compile from scratch. The VM is recreated
//! each turn.
//!
//! That's slow per character but correct under every Lex invariant —
//! incremental compilation in the VM is its own project. For human
//! REPL pace (1 input / second) it's invisible.
//!
//! Multi-line input: lines accumulate until brace-depth returns to
//! 0 (and the line isn't blank). A blank line cancels the
//! current input. `.help`, `.quit`, `.reset`, `.list` are meta
//! commands.

use anyhow::{anyhow, Result};
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::io::{BufRead, Write};

const BANNER: &str = "lex repl — type .help for commands, .quit to exit";

pub fn cmd_repl(_args: &[String]) -> Result<()> {
    println!("{BANNER}");
    let mut session = Session::new();
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = String::new();

    loop {
        // Multi-line read: keep pulling lines until brace-balanced.
        buf.clear();
        let mut depth: i64 = 0;
        let mut first = true;
        loop {
            print!("{}", if first { "lex> " } else { ".... " });
            std::io::stdout().flush().ok();
            let mut line = String::new();
            let n = stdin.read_line(&mut line).map_err(|e| anyhow!("stdin: {e}"))?;
            if n == 0 {
                println!();
                return Ok(());
            }
            // Blank line on the first prompt = noop; on continuation lines = abort.
            if line.trim().is_empty() {
                if first { continue; } else { buf.clear(); break; }
            }
            first = false;
            depth += brace_balance(&line);
            buf.push_str(&line);
            if depth <= 0 { break; }
        }
        let input = buf.trim();
        if input.is_empty() { continue; }

        // Meta commands.
        if let Some(rest) = input.strip_prefix('.') {
            match rest.trim() {
                "help" => print_help(),
                "quit" | "exit" => return Ok(()),
                "reset" => { session = Session::new(); println!("(session cleared)"); }
                "list" => println!("{}", session.stages),
                other => println!("unknown meta command `.{other}` (try .help)"),
            }
            continue;
        }

        // Try as a stage first (fn/type/import); fall back to expression.
        if looks_like_stage(input) {
            session.add_stage(input);
            match session.check() {
                Ok(_) => println!("(ok)"),
                Err(e) => {
                    println!("error: {e}");
                    session.rollback();
                }
            }
        } else {
            match session.eval_expr(input) {
                Ok(s) => println!("=> {s}"),
                Err(e) => println!("error: {e}"),
            }
        }
    }
}

fn print_help() {
    println!("Commands:");
    println!("  .help       this message");
    println!("  .quit       exit the REPL");
    println!("  .reset      drop all defined stages, start over");
    println!("  .list       print the current accumulated source");
    println!();
    println!("Top-level inputs (`fn …`, `type …`, `import …`) are added");
    println!("to the session. Anything else is evaluated as an expression");
    println!("under a permissive policy (all effects allowed); use the");
    println!("CLI's `lex run` for policy-gated execution.");
}

/// Heuristic: `fn`, `type`, `import` lead a stage. Everything else
/// is an expression.
fn looks_like_stage(s: &str) -> bool {
    let head = s.split_whitespace().next().unwrap_or("");
    matches!(head, "fn" | "type" | "import")
}

/// Net `{`s minus `}`s on a line. Lets the prompt continue into a
/// multi-line definition.
fn brace_balance(line: &str) -> i64 {
    let mut d: i64 = 0;
    for ch in line.chars() {
        match ch {
            '{' => d += 1,
            '}' => d -= 1,
            _ => {}
        }
    }
    d
}

struct Session {
    /// Accumulated Lex source. New stages append; rollback truncates.
    stages: String,
    /// Snapshots so we can roll back failed adds.
    history: Vec<usize>,
    /// Counter for unique `replEval<n>` names.
    eval_count: u32,
}

impl Session {
    fn new() -> Self { Self { stages: String::new(), history: Vec::new(), eval_count: 0 } }

    fn add_stage(&mut self, src: &str) {
        self.history.push(self.stages.len());
        self.stages.push_str(src);
        self.stages.push('\n');
    }

    fn rollback(&mut self) {
        if let Some(len) = self.history.pop() {
            self.stages.truncate(len);
        }
    }

    fn check(&self) -> Result<()> {
        let prog = parse_source(&self.stages).map_err(|e| anyhow!("parse: {e}"))?;
        let stages = canonicalize_program(&prog);
        if let Err(errs) = lex_types::check_program(&stages) {
            let lines: Vec<String> = errs.iter()
                .map(|e| serde_json::to_string(e).unwrap_or_else(|_| format!("{e:?}")))
                .collect();
            return Err(anyhow!(lines.join("\n")));
        }
        Ok(())
    }

    fn eval_expr(&mut self, expr: &str) -> Result<String> {
        // Wrap the expression in a fresh fn that returns Str via
        // `json.stringify(<expr>)`. `json.stringify` is polymorphic
        // (`T -> Str`), so the wrapper accepts any expression type
        // and we get a printable result back. The wrapper is
        // ephemeral — each eval gets a fresh name, so `.list` shows
        // only stages the user added explicitly.
        //
        // We auto-inject `import "std.json" as json` at the top of
        // the program if the user hasn't already, so REPL eval works
        // before any explicit imports.
        self.eval_count += 1;
        let name = format!("replEval{}", self.eval_count);
        let needs_json_import = !self.stages.contains("std.json");
        let preamble = if needs_json_import {
            "import \"std.json\" as json\n".to_string()
        } else {
            String::new()
        };
        let wrapped = format!("\nfn {name}() -> Str {{ json.stringify({expr}) }}\n");
        let combined = format!("{preamble}{}{wrapped}", self.stages);
        let prog = parse_source(&combined).map_err(|e| anyhow!("parse: {e}"))?;
        let stages = canonicalize_program(&prog);
        if let Err(errs) = lex_types::check_program(&stages) {
            let lines: Vec<String> = errs.iter()
                .map(|e| serde_json::to_string(e).unwrap_or_else(|_| format!("{e:?}")))
                .collect();
            return Err(anyhow!(lines.join("\n")));
        }
        let bc = compile_program(&stages);
        let policy = Policy::permissive();
        check_policy(&bc, &policy).map_err(|v| anyhow!(format!("{v:?}")))?;
        let bc = std::sync::Arc::new(bc);
        let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let v = vm.call(&name, vec![]).map_err(|e| anyhow!("runtime: {e}"))?;
        // The wrapper returns a Str (the JSON encoding of the
        // original value). Unwrap one layer so the user sees the
        // JSON directly, not a quoted string.
        match v {
            lex_bytecode::Value::Str(s) => Ok(s),
            other => Ok(serde_json::to_string(&other.to_json())
                .unwrap_or_else(|_| format!("{other:?}"))),
        }
    }
}
