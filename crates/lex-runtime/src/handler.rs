//! Native effect handlers, dispatched at runtime through the VM's
//! `EffectHandler` trait. The handler also re-checks the runtime policy
//! per spec §7.4 (the static check is necessary but not sufficient: a fn
//! declared `[fs_read("/data")]` that's allowed at startup still has to
//! pass the path check at the point of dispatch).

use lex_bytecode::vm::{EffectHandler, Vm};
use lex_bytecode::{Program, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::builtins::try_pure_builtin;
use crate::policy::Policy;

/// Output sink used by `io.print`. Tests inject a buffer; production prints
/// to stdout.
pub trait IoSink: Send {
    fn print_line(&mut self, s: &str);
}

pub struct StdoutSink;
impl IoSink for StdoutSink {
    fn print_line(&mut self, s: &str) {
        println!("{s}");
    }
}

#[derive(Default)]
pub struct CapturedSink { pub lines: Vec<String> }
impl IoSink for CapturedSink {
    fn print_line(&mut self, s: &str) { self.lines.push(s.to_string()); }
}

pub struct DefaultHandler {
    policy: Policy,
    pub sink: Box<dyn IoSink>,
    /// Optional read root for `io.read` — when set, `io.read("p")` resolves
    /// to `read_root.join(p)`. Lets tests run without touching the real fs.
    pub read_root: Option<PathBuf>,
    /// Per-run budget pool (#225). `Arc<AtomicU64>` so parallel
    /// branches share one counter without locking. Initialized to
    /// the policy ceiling at handler construction; each call to a
    /// function with declared `[budget(N)]` deducts N atomically
    /// via `note_call_budget`. Cloning the handler is intentional
    /// for net.serve / chat handlers — they share the same pool.
    pub budget_remaining: Arc<AtomicU64>,
    /// The original ceiling that `budget_remaining` started at, kept
    /// for diagnostics so a `BudgetExceeded` error can report
    /// `(used, ceiling)` rather than just "exceeded by N".
    pub budget_ceiling: Option<u64>,
    /// Shared reference to the program, needed by `net.serve` so the
    /// handler can spin up fresh VMs to dispatch incoming requests.
    /// `None` if the handler was constructed without a program.
    pub program: Option<Arc<Program>>,
    /// Chat registry; populated by `net.serve_ws`'s per-message
    /// dispatch so `chat.broadcast` / `chat.send` work from inside
    /// a handler invocation.
    pub chat_registry: Option<Arc<crate::ws::ChatRegistry>>,
    /// LRU cache of `agent.call_mcp` clients keyed by the
    /// command-line string (#197). Avoids spawn-per-call cost
    /// when an agent invokes the same MCP server in tight loops.
    /// Capped — when the cache is full, the least-recently-used
    /// entry is dropped (its subprocess is reaped on Drop).
    pub mcp_clients: crate::mcp_client::McpClientCache,
}

impl DefaultHandler {
    pub fn new(policy: Policy) -> Self {
        // If the caller supplied a ceiling, the pool starts at that
        // ceiling and counts down. No ceiling = `u64::MAX` so calls
        // never refuse on budget grounds (existing behavior).
        let ceiling = policy.budget;
        let initial = ceiling.unwrap_or(u64::MAX);
        Self {
            policy,
            sink: Box::new(StdoutSink),
            read_root: None,
            budget_remaining: Arc::new(AtomicU64::new(initial)),
            budget_ceiling: ceiling,
            program: None,
            chat_registry: None,
            mcp_clients: crate::mcp_client::McpClientCache::with_capacity(16),
        }
    }

    pub fn with_program(mut self, program: Arc<Program>) -> Self {
        self.program = Some(program); self
    }

    pub fn with_chat_registry(mut self, registry: Arc<crate::ws::ChatRegistry>) -> Self {
        self.chat_registry = Some(registry); self
    }

    pub fn with_sink(mut self, sink: Box<dyn IoSink>) -> Self {
        self.sink = sink; self
    }

    pub fn with_read_root(mut self, root: PathBuf) -> Self {
        self.read_root = Some(root); self
    }

    fn ensure_kind_allowed(&self, kind: &str) -> Result<(), String> {
        if self.policy.allow_effects.contains(kind) {
            Ok(())
        } else {
            Err(format!("effect `{kind}` not in --allow-effects"))
        }
    }

    fn resolve_read_path(&self, p: &str) -> PathBuf {
        match &self.read_root {
            Some(root) => root.join(p.trim_start_matches('/')),
            None => PathBuf::from(p),
        }
    }

    fn dispatch_log(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "debug" | "info" | "warn" | "error" => {
                let msg = expect_str(args.first())?;
                let level = match op {
                    "debug" => LogLevel::Debug,
                    "info" => LogLevel::Info,
                    "warn" => LogLevel::Warn,
                    _ => LogLevel::Error,
                };
                emit_log(level, msg);
                Ok(Value::Unit)
            }
            "set_level" => {
                let s = expect_str(args.first())?;
                match parse_log_level(s) {
                    Some(l) => {
                        log_state().lock().unwrap().level = l;
                        Ok(ok(Value::Unit))
                    }
                    None => Ok(err(Value::Str(format!(
                        "log.set_level: unknown level `{s}`; expected debug|info|warn|error")))),
                }
            }
            "set_format" => {
                let s = expect_str(args.first())?;
                let fmt = match s {
                    "text" => LogFormat::Text,
                    "json" => LogFormat::Json,
                    other => return Ok(err(Value::Str(format!(
                        "log.set_format: unknown format `{other}`; expected text|json")))),
                };
                log_state().lock().unwrap().format = fmt;
                Ok(ok(Value::Unit))
            }
            "set_sink" => {
                let path = expect_str(args.first())?;
                if path == "-" {
                    log_state().lock().unwrap().sink = LogSink::Stderr;
                    return Ok(ok(Value::Unit));
                }
                if let Err(e) = self.ensure_fs_write_path(path) {
                    return Ok(err(Value::Str(e)));
                }
                match std::fs::OpenOptions::new()
                    .create(true).append(true).open(path)
                {
                    Ok(f) => {
                        log_state().lock().unwrap().sink = LogSink::File(std::sync::Arc::new(Mutex::new(f)));
                        Ok(ok(Value::Unit))
                    }
                    Err(e) => Ok(err(Value::Str(format!("log.set_sink `{path}`: {e}")))),
                }
            }
            other => Err(format!("unsupported log.{other}")),
        }
    }

    fn dispatch_process(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "spawn" => {
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items.clone(),
                    _ => return Err("process.spawn: args must be List[Str]".into()),
                };
                let str_args: Result<Vec<String>, String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(format!("process.spawn: arg must be Str, got {other:?}")),
                }).collect();
                let str_args = str_args?;
                let opts = match args.get(2) {
                    Some(Value::Record(r)) => r.clone(),
                    _ => return Err("process.spawn: missing or invalid opts record".into()),
                };

                // Allow-list check, mirroring the existing proc.spawn.
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "process.spawn: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ))));
                    }
                }

                let mut command = std::process::Command::new(&cmd);
                command.args(&str_args);
                command.stdin(std::process::Stdio::piped());
                command.stdout(std::process::Stdio::piped());
                command.stderr(std::process::Stdio::piped());

                if let Some(Value::Variant { name, args: vargs }) = opts.get("cwd") {
                    if name == "Some" {
                        if let Some(Value::Str(s)) = vargs.first() {
                            command.current_dir(s);
                        }
                    }
                }
                if let Some(Value::Map(env)) = opts.get("env") {
                    for (k, v) in env {
                        if let (lex_bytecode::MapKey::Str(ks), Value::Str(vs)) = (k, v) {
                            command.env(ks, vs);
                        }
                    }
                }

                let stdin_payload: Option<Vec<u8>> = match opts.get("stdin") {
                    Some(Value::Variant { name, args: vargs }) if name == "Some" => {
                        match vargs.first() {
                            Some(Value::Bytes(b)) => Some(b.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                };

                let mut child = match command.spawn() {
                    Ok(c) => c,
                    Err(e) => return Ok(err(Value::Str(format!("process.spawn `{cmd}`: {e}")))),
                };

                if let Some(payload) = stdin_payload {
                    if let Some(mut stdin) = child.stdin.take() {
                        use std::io::Write;
                        let _ = stdin.write_all(&payload);
                        // Drop closes stdin; the child sees EOF.
                    }
                }

                let stdout = child.stdout.take().map(std::io::BufReader::new);
                let stderr = child.stderr.take().map(std::io::BufReader::new);
                let handle = next_process_handle();
                process_registry().lock().unwrap().insert(handle, ProcessState {
                    child,
                    stdout,
                    stderr,
                });
                Ok(ok(Value::Int(handle as i64)))
            }
            "read_stdout_line" => Self::read_line_op(args, true),
            "read_stderr_line" => Self::read_line_op(args, false),
            "wait" => {
                let h = expect_process_handle(args.first())?;
                // Look up the per-handle Arc, then release the global
                // lock before the (slow) wait so unrelated handles
                // can dispatch concurrently.
                let arc = process_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "process.wait: closed or unknown ProcessHandle".to_string())?;
                let status = {
                    let mut state = arc.lock().unwrap();
                    state.child.wait().map_err(|e| format!("process.wait: {e}"))?
                };
                // Wait completion makes the handle terminal; drop it
                // from the registry so the cap doesn't fill up with
                // exited children.
                process_registry().lock().unwrap().remove(h);
                let mut rec = indexmap::IndexMap::new();
                rec.insert("code".into(), Value::Int(status.code().unwrap_or(-1) as i64));
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    rec.insert("signaled".into(), Value::Bool(status.signal().is_some()));
                }
                #[cfg(not(unix))]
                {
                    rec.insert("signaled".into(), Value::Bool(false));
                }
                Ok(Value::Record(rec))
            }
            "kill" => {
                let h = expect_process_handle(args.first())?;
                let _signal = expect_str(args.get(1))?;
                let arc = process_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "process.kill: closed or unknown ProcessHandle".to_string())?;
                let mut state = arc.lock().unwrap();
                // Cross-platform: only `kill` (SIGKILL-equivalent on
                // Windows). Signal-name dispatch is a v1.5 follow-up.
                match state.child.kill() {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("process.kill: {e}")))),
                }
            }
            "run" => {
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items.clone(),
                    _ => return Err("process.run: args must be List[Str]".into()),
                };
                let str_args: Result<Vec<String>, String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(format!("process.run: arg must be Str, got {other:?}")),
                }).collect();
                let str_args = str_args?;
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "process.run: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ))));
                    }
                }
                match std::process::Command::new(&cmd).args(&str_args).output() {
                    Ok(o) => {
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("stdout".into(), Value::Str(
                            String::from_utf8_lossy(&o.stdout).to_string()));
                        rec.insert("stderr".into(), Value::Str(
                            String::from_utf8_lossy(&o.stderr).to_string()));
                        rec.insert("exit_code".into(), Value::Int(
                            o.status.code().unwrap_or(-1) as i64));
                        Ok(ok(Value::Record(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("process.run `{cmd}`: {e}")))),
                }
            }
            other => Err(format!("unsupported process.{other}")),
        }
    }

    /// Read one line from the child's stdout (`is_stdout = true`) or
    /// stderr. Returns `None` (Lex `Option`) at EOF; subsequent calls
    /// keep returning `None`. Holds only the per-handle mutex during
    /// the (potentially blocking) read, so reads on one handle don't
    /// block reads/waits on a different handle.
    fn read_line_op(args: Vec<Value>, is_stdout: bool) -> Result<Value, String> {
        let h = expect_process_handle(args.first())?;
        let arc = process_registry().lock().unwrap()
            .touch_get(h)
            .ok_or_else(|| format!(
                "process.read_{}_line: closed or unknown ProcessHandle",
                if is_stdout { "stdout" } else { "stderr" }))?;
        let mut state = arc.lock().unwrap();
        let reader_opt = if is_stdout {
            state.stdout.as_mut().map(|r| -> &mut dyn std::io::BufRead { r })
        } else {
            state.stderr.as_mut().map(|r| -> &mut dyn std::io::BufRead { r })
        };
        let reader = match reader_opt {
            Some(r) => r,
            None => return Ok(none()),
        };
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => Ok(none()),
            Ok(_) => {
                if line.ends_with('\n') { line.pop(); }
                if line.ends_with('\r') { line.pop(); }
                Ok(some(Value::Str(line)))
            }
            Err(e) => Err(format!("process.read_*_line: {e}")),
        }
    }

    fn dispatch_fs(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "exists" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                Ok(Value::Bool(std::path::Path::new(&path).exists()))
            }
            "is_file" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_file()))
            }
            "is_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                Ok(Value::Bool(std::path::Path::new(&path).is_dir()))
            }
            "stat" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                match std::fs::metadata(&path) {
                    Ok(md) => {
                        let mtime = md.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("size".into(), Value::Int(md.len() as i64));
                        rec.insert("mtime".into(), Value::Int(mtime));
                        rec.insert("is_dir".into(), Value::Bool(md.is_dir()));
                        rec.insert("is_file".into(), Value::Bool(md.is_file()));
                        Ok(ok(Value::Record(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.stat `{path}`: {e}")))),
                }
            }
            "list_dir" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut entries: Vec<Value> = Vec::new();
                        for ent in rd {
                            match ent {
                                Ok(e) => {
                                    let p = e.path();
                                    entries.push(Value::Str(p.to_string_lossy().into_owned()));
                                }
                                Err(e) => return Ok(err(Value::Str(format!("fs.list_dir: {e}")))),
                            }
                        }
                        Ok(ok(Value::List(entries)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("fs.list_dir `{path}`: {e}")))),
                }
            }
            "walk" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                let mut paths: Vec<Value> = Vec::new();
                for ent in walkdir::WalkDir::new(&path) {
                    match ent {
                        Ok(e) => paths.push(Value::Str(
                            e.path().to_string_lossy().into_owned())),
                        Err(e) => return Ok(err(Value::Str(format!("fs.walk: {e}")))),
                    }
                }
                Ok(ok(Value::List(paths)))
            }
            "glob" => {
                let pattern = expect_str(args.first())?.to_string();
                // Glob patterns can't be path-scoped at parse time
                // (`**/*.rs` doesn't pin a directory); we filter the
                // per-result paths after expansion against
                // `--allow-fs-read`.
                let entries = match glob::glob(&pattern) {
                    Ok(e) => e,
                    Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}")))),
                };
                let mut paths: Vec<Value> = Vec::new();
                for ent in entries {
                    match ent {
                        Ok(p) => {
                            let s = p.to_string_lossy().into_owned();
                            if self.policy.allow_fs_read.is_empty()
                                || self.policy.allow_fs_read.iter().any(|root| p.starts_with(root))
                            {
                                paths.push(Value::Str(s));
                            }
                        }
                        Err(e) => return Ok(err(Value::Str(format!("fs.glob: {e}")))),
                    }
                }
                Ok(ok(Value::List(paths)))
            }
            "mkdir_p" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                match std::fs::create_dir_all(&path) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.mkdir_p `{path}`: {e}")))),
                }
            }
            "remove" => {
                let path = expect_str(args.first())?.to_string();
                if let Err(e) = self.ensure_fs_write_path(&path) {
                    return Ok(err(Value::Str(e)));
                }
                let p = std::path::Path::new(&path);
                let result = if p.is_dir() {
                    std::fs::remove_dir_all(p)
                } else {
                    std::fs::remove_file(p)
                };
                match result {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.remove `{path}`: {e}")))),
                }
            }
            "copy" => {
                let src = expect_str(args.first())?.to_string();
                let dst = expect_str(args.get(1))?.to_string();
                if let Err(e) = self.ensure_fs_walk_path(&src) {
                    return Ok(err(Value::Str(e)));
                }
                if let Err(e) = self.ensure_fs_write_path(&dst) {
                    return Ok(err(Value::Str(e)));
                }
                match std::fs::copy(&src, &dst) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("fs.copy {src} -> {dst}: {e}")))),
                }
            }
            other => Err(format!("unsupported fs.{other}")),
        }
    }

    /// Path scope for walk-style operations. `[fs_walk]` reuses the
    /// `--allow-fs-read` allowlist — listing a directory is an
    /// information disclosure on the same path tree as reading file
    /// content, so the same scope applies. Empty allowlist = any path.
    fn ensure_fs_walk_path(&self, path: &str) -> Result<(), String> {
        if self.policy.allow_fs_read.is_empty() {
            return Ok(());
        }
        let p = std::path::Path::new(path);
        if self.policy.allow_fs_read.iter().any(|a| p.starts_with(a)) {
            Ok(())
        } else {
            Err(format!("fs path `{path}` outside --allow-fs-read"))
        }
    }

    /// Path scope for mutating operations. `[fs_write]` uses the
    /// existing `--allow-fs-write` allowlist.
    fn ensure_fs_write_path(&self, path: &str) -> Result<(), String> {
        if self.policy.allow_fs_write.is_empty() {
            return Ok(());
        }
        let p = std::path::Path::new(path);
        if self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
            Ok(())
        } else {
            Err(format!("fs path `{path}` outside --allow-fs-write"))
        }
    }

    /// Enforce `--allow-net-host` against an outgoing URL. Empty
    /// allowlist = any host. Non-empty = the URL's host must match
    /// (substring; port-agnostic) at least one entry.
    fn ensure_host_allowed(&self, url: &str) -> Result<(), String> {
        if self.policy.allow_net_host.is_empty() { return Ok(()); }
        let host = extract_host(url).unwrap_or("");
        if self.policy.allow_net_host.iter().any(|h| host == h) {
            Ok(())
        } else {
            Err(format!(
                "net call to host `{host}` not in --allow-net-host {:?}",
                self.policy.allow_net_host,
            ))
        }
    }
}

fn extract_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))?;
    let host_port = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    Some(match host_port.rsplit_once(':') {
        Some((h, _)) => h,
        None => host_port,
    })
}

impl EffectHandler for DefaultHandler {
    /// Per-call budget enforcement (#225). VM calls this before
    /// invoking any function whose signature declares `[budget(N)]`.
    /// The cost N is deducted atomically from the shared pool;
    /// returning `Err` aborts the call before any frame is pushed.
    fn note_call_budget(&mut self, cost: u64) -> Result<(), String> {
        // Skip the work entirely when no ceiling is configured —
        // the pool is `u64::MAX` and would never trip.
        let Some(ceiling) = self.budget_ceiling else { return Ok(()); };
        // Compare-and-swap: speculatively subtract; if we'd
        // underflow, return BudgetExceeded without mutating.
        // Use SeqCst because parallel branches may race here and
        // the relative ordering of "used so far" vs. "this call's
        // cost" needs to be deterministic across threads.
        loop {
            let cur = self.budget_remaining.load(Ordering::SeqCst);
            if cost > cur {
                let used = ceiling.saturating_sub(cur);
                return Err(format!(
                    "budget exceeded: requested {cost}, used so far {used}, ceiling {ceiling}"));
            }
            let next = cur - cost;
            // Conservative accounting: if the CAS races and loses,
            // re-read and try again. No refund-on-failure path.
            if self.budget_remaining.compare_exchange(cur, next,
                Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                return Ok(());
            }
        }
    }

    fn dispatch(&mut self, kind: &str, op: &str, args: Vec<Value>) -> Result<Value, String> {
        // Pure stdlib builtins (str, list, json, ...) bypass the policy
        // gate — they have no observable side effects and aren't tracked
        // by the type system as effects.
        if let Some(r) = try_pure_builtin(kind, op, &args) {
            return r;
        }
        // `std.fs` ops use the fine-grained `[fs_walk]` and `[fs_write]`
        // effect kinds (distinct from the module name `fs`); the
        // policy check uses the per-op kind, not the module's.
        if kind == "process" {
            self.ensure_kind_allowed("proc")?;
            return self.dispatch_process(op, args);
        }
        if kind == "log" {
            // Emit ops are [log]; config ops are [io] (set_sink also
            // [fs_write]). The dispatch picks the right kind per op.
            let effect_kind = match op {
                "debug" | "info" | "warn" | "error" => "log",
                "set_level" | "set_format" => "io",
                "set_sink" => {
                    self.ensure_kind_allowed("io")?;
                    self.ensure_kind_allowed("fs_write")?;
                    return self.dispatch_log(op, args);
                }
                other => return Err(format!("unsupported log.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            return self.dispatch_log(op, args);
        }
        if kind == "fs" {
            let effect_kind = match op {
                "exists" | "is_file" | "is_dir" | "stat"
                | "list_dir" | "walk" | "glob" => "fs_walk",
                "mkdir_p" | "remove" => "fs_write",
                "copy" => {
                    self.ensure_kind_allowed("fs_walk")?;
                    self.ensure_kind_allowed("fs_write")?;
                    return self.dispatch_fs(op, args);
                }
                other => return Err(format!("unsupported fs.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            return self.dispatch_fs(op, args);
        }
        // `crypto.random` is the lone effectful op in `std.crypto`. Its
        // declared effect kind is `random` (fine-grained on purpose so
        // `lex audit --effect random` flags every token-generating
        // call), distinct from the `crypto` module name.
        // datetime.now is the only effectful op in std.datetime;
        // declared kind is `time`, matching the existing `time.now`.
        if kind == "datetime" && op == "now" {
            self.ensure_kind_allowed("time")?;
            let now = chrono::Utc::now();
            let nanos = now.timestamp_nanos_opt().unwrap_or(i64::MAX);
            return Ok(Value::Int(nanos));
        }
        if kind == "crypto" && op == "random" {
            self.ensure_kind_allowed("random")?;
            let n = expect_int(args.first())?;
            if !(0..=1_048_576).contains(&n) {
                return Err("crypto.random: n must be in 0..=1048576".into());
            }
            use rand::{rngs::OsRng, TryRngCore};
            let mut buf = vec![0u8; n as usize];
            OsRng.try_fill_bytes(&mut buf)
                .map_err(|e| format!("crypto.random: OS RNG: {e}"))?;
            return Ok(Value::Bytes(buf));
        }
        // `std.http` wire ops (send/get/post) gate on the `net`
        // effect kind, not the module name. This matches the
        // declared signature (`http.get :: Str -> [net] ...`) and
        // keeps `--allow-effects net` doing the obvious thing for
        // both `net.*` and `http.*` callers.
        // `std.agent` (#184): the four runtime effects added for
        // agent-style programs (`llm_local`, `llm_cloud`, `a2a`,
        // `mcp`). The handlers are stubs — they enforce the
        // declared-effect gate, return a sentinel `Ok` so traces
        // record the call, and defer the real wire formats to
        // downstream crates (`soft-agent` for `llm_*` and `a2a`)
        // and #185 (MCP client wrapper).
        if kind == "agent" {
            let effect_kind = match op {
                "local_complete" => "llm_local",
                "cloud_complete" => "llm_cloud",
                "send_a2a"       => "a2a",
                "call_mcp"       => "mcp",
                other => return Err(format!("unsupported agent.{other}")),
            };
            self.ensure_kind_allowed(effect_kind)?;
            // `call_mcp` runs through the LRU client cache
            // (#197). `local_complete` / `cloud_complete` hit
            // Ollama / OpenAI via env-var-driven configuration
            // (#196); custom backends override at the
            // EffectHandler layer rather than via a config file.
            // `send_a2a` keeps its stub — that wire format
            // lives in downstream `soft-a2a`.
            return match op {
                "call_mcp"       => Ok(self.dispatch_call_mcp(args)),
                "local_complete" => Ok(dispatch_llm_local(args)),
                "cloud_complete" => Ok(dispatch_llm_cloud(args)),
                _ => Ok(ok(Value::Str(format!("<{effect_kind} stub>")))),
            };
        }
        if kind == "http" && matches!(op, "send" | "get" | "post") {
            self.ensure_kind_allowed("net")?;
            return match op {
                "send" => {
                    let req = expect_record(args.first())?;
                    Ok(http_send_record(self, req))
                }
                "get" => {
                    let url = expect_str(args.first())?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_send_simple("GET", &url, None, "", None))
                }
                "post" => {
                    let url = expect_str(args.first())?.to_string();
                    let body = expect_bytes(args.get(1))?.clone();
                    let content_type = expect_str(args.get(2))?.to_string();
                    self.ensure_host_allowed(&url)?;
                    Ok(http_send_simple("POST", &url, Some(body), &content_type, None))
                }
                _ => unreachable!(),
            };
        }
        self.ensure_kind_allowed(kind)?;
        match (kind, op) {
            ("io", "print") => {
                let line = expect_str(args.first())?;
                self.sink.print_line(line);
                Ok(Value::Unit)
            }
            ("io", "read") => {
                let path = expect_str(args.first())?.to_string();
                let resolved = self.resolve_read_path(&path);
                // Honor read-allowlist if any. Symmetric with io.write.
                // The path argument is checked as-given (resolved-against-
                // read_root for tests); a tool granted [io] cannot escape
                // the configured prefix even though the effect itself is
                // permitted. This is the per-path scope the bench's case
                // #6 ("[io] granted, body reads /etc/passwd") needed.
                if !self.policy.allow_fs_read.is_empty()
                    && !self.policy.allow_fs_read.iter().any(|a| resolved.starts_with(a))
                {
                    return Err(format!("read of `{path}` outside --allow-fs-read"));
                }
                match std::fs::read_to_string(&resolved) {
                    Ok(s) => Ok(ok(Value::Str(s))),
                    Err(e) => Ok(err(Value::Str(format!("{e}")))),
                }
            }
            ("io", "write") => {
                let path = expect_str(args.first())?.to_string();
                let contents = expect_str(args.get(1))?.to_string();
                // Honor write-allowlist if any.
                if !self.policy.allow_fs_write.is_empty() {
                    let p = std::path::Path::new(&path);
                    if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                        return Err(format!("write to `{path}` outside --allow-fs-write"));
                    }
                }
                match std::fs::write(&path, contents) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("{e}")))),
                }
            }
            ("time", "now") => {
                let secs = SystemTime::now().duration_since(UNIX_EPOCH)
                    .map_err(|e| format!("time: {e}"))?.as_secs();
                Ok(Value::Int(secs as i64))
            }
            ("time", "sleep_ms") => {
                // Block the current thread for `n` ms (#226). Used
                // by `flow.retry_with_backoff`'s exponential delay.
                // Negative or zero is a no-op. Bounded at 60s in the
                // runtime to avoid pathological agent-emitted loops
                // wedging the host — anything legitimate beyond
                // that should use process-level scheduling, not a
                // blocking sleep.
                let n = expect_int(args.first())?;
                if n > 0 {
                    let ms = (n as u64).min(60_000);
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                Ok(Value::Unit)
            }
            ("rand", "int_in") => {
                // Deterministic stub: midpoint of [lo, hi].
                let lo = expect_int(args.first())?;
                let hi = expect_int(args.get(1))?;
                Ok(Value::Int((lo + hi) / 2))
            }
            // `env.get` returns `Option[Str]` — `None` for unset vars.
            // Per-var scoping (`[env(NAME)]`) arrives with #207's
            // per-capability effect parameterization; today the flat
            // `[env]` grants access to the entire process environment.
            ("env", "get") => {
                let name = expect_str(args.first())?;
                Ok(match std::env::var(&name) {
                    Ok(v) => Value::Variant {
                        name: "Some".into(),
                        args: vec![Value::Str(v)],
                    },
                    Err(_) => Value::Variant { name: "None".into(), args: Vec::new() },
                })
            }
            ("budget", _) => {
                // Budget calls are nominally tracked here; budget itself is
                // enforced statically in `policy::check_program`.
                Ok(Value::Unit)
            }
            ("net", "get") => {
                let url = expect_str(args.first())?.to_string();
                self.ensure_host_allowed(&url)?;
                Ok(http_request("GET", &url, None))
            }
            ("net", "post") => {
                let url = expect_str(args.first())?.to_string();
                let body = expect_str(args.get(1))?.to_string();
                self.ensure_host_allowed(&url)?;
                Ok(http_request("POST", &url, Some(&body)))
            }
            ("net", "serve") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve(port, handler): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve requires a Program reference; use DefaultHandler::with_program".to_string())?;
                let policy = self.policy.clone();
                serve_http(port, handler_name, program, policy, None)
            }
            ("net", "serve_tls") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_tls(port, cert, key, handler): port must be Int 0..=65535".into()),
                };
                let cert_path = expect_str(args.get(1))?.to_string();
                let key_path = expect_str(args.get(2))?.to_string();
                let handler_name = expect_str(args.get(3))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_tls requires a Program reference".to_string())?;
                let policy = self.policy.clone();
                let cert = std::fs::read(&cert_path)
                    .map_err(|e| format!("net.serve_tls: read cert {cert_path}: {e}"))?;
                let key = std::fs::read(&key_path)
                    .map_err(|e| format!("net.serve_tls: read key {key_path}: {e}"))?;
                serve_http(port, handler_name, program, policy, Some(TlsConfig { cert, key }))
            }
            ("net", "serve_ws") => {
                let port = match args.first() {
                    Some(Value::Int(n)) if (0..=65535).contains(n) => *n as u16,
                    _ => return Err("net.serve_ws(port, on_message): port must be Int 0..=65535".into()),
                };
                let handler_name = expect_str(args.get(1))?.to_string();
                let program = self.program.clone()
                    .ok_or_else(|| "net.serve_ws requires a Program reference".to_string())?;
                let policy = self.policy.clone();
                let registry = Arc::new(crate::ws::ChatRegistry::default());
                crate::ws::serve_ws(port, handler_name, program, policy, registry)
            }
            ("chat", "broadcast") => {
                let registry = self.chat_registry.as_ref()
                    .ok_or_else(|| "chat.broadcast called outside a net.serve_ws handler".to_string())?;
                let room = expect_str(args.first())?;
                let body = expect_str(args.get(1))?;
                crate::ws::chat_broadcast(registry, room, body);
                Ok(Value::Unit)
            }
            ("chat", "send") => {
                let registry = self.chat_registry.as_ref()
                    .ok_or_else(|| "chat.send called outside a net.serve_ws handler".to_string())?;
                let conn_id = match args.first() {
                    Some(Value::Int(n)) if *n >= 0 => *n as u64,
                    _ => return Err("chat.send: conn_id must be non-negative Int".into()),
                };
                let body = expect_str(args.get(1))?;
                Ok(Value::Bool(crate::ws::chat_send(registry, conn_id, body)))
            }
            ("kv", "open") => {
                let path = expect_str(args.first())?.to_string();
                // Honor write-allowlist: opening a Kv writes its
                // backing files at `path`, so the same scoping that
                // applies to `io.write` applies here.
                if !self.policy.allow_fs_write.is_empty() {
                    let p = std::path::Path::new(&path);
                    if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                        return Ok(err(Value::Str(format!(
                            "kv.open: `{path}` outside --allow-fs-write"))));
                    }
                }
                match sled::open(&path) {
                    Ok(db) => {
                        let handle = next_kv_handle();
                        kv_registry().lock().unwrap().insert(handle, db);
                        Ok(ok(Value::Int(handle as i64)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("kv.open: {e}")))),
                }
            }
            ("kv", "close") => {
                let h = expect_kv_handle(args.first())?;
                kv_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("kv", "get") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.get: closed or unknown Kv handle".to_string())?;
                match db.get(key.as_bytes()) {
                    Ok(Some(ivec)) => Ok(some(Value::Bytes(ivec.to_vec()))),
                    Ok(None) => Ok(none()),
                    Err(e) => Err(format!("kv.get: {e}")),
                }
            }
            ("kv", "put") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?.to_string();
                let val = expect_bytes(args.get(2))?.clone();
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.put: closed or unknown Kv handle".to_string())?;
                match db.insert(key.as_bytes(), val) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.put: {e}")))),
                }
            }
            ("kv", "delete") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.delete: closed or unknown Kv handle".to_string())?;
                match db.remove(key.as_bytes()) {
                    Ok(_) => Ok(ok(Value::Unit)),
                    Err(e) => Ok(err(Value::Str(format!("kv.delete: {e}")))),
                }
            }
            ("kv", "contains") => {
                let h = expect_kv_handle(args.first())?;
                let key = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.contains: closed or unknown Kv handle".to_string())?;
                match db.contains_key(key.as_bytes()) {
                    Ok(present) => Ok(Value::Bool(present)),
                    Err(e) => Err(format!("kv.contains: {e}")),
                }
            }
            ("kv", "list_prefix") => {
                let h = expect_kv_handle(args.first())?;
                let prefix = expect_str(args.get(1))?;
                let mut reg = kv_registry().lock().unwrap();
                let db = reg.touch_get(h).ok_or_else(|| "kv.list_prefix: closed or unknown Kv handle".to_string())?;
                let mut keys: Vec<Value> = Vec::new();
                for kv in db.scan_prefix(prefix.as_bytes()) {
                    let (k, _) = kv.map_err(|e| format!("kv.list_prefix: {e}"))?;
                    let s = String::from_utf8_lossy(&k).to_string();
                    keys.push(Value::Str(s));
                }
                Ok(Value::List(keys))
            }
            ("sql", "open") => {
                let path = expect_str(args.first())?.to_string();
                // Same shape as `kv.open`: opening creates the
                // SQLite file, so the fs-write allowlist applies
                // (in-memory paths are exempt).
                if path != ":memory:" && !self.policy.allow_fs_write.is_empty() {
                    let p = std::path::Path::new(&path);
                    if !self.policy.allow_fs_write.iter().any(|a| p.starts_with(a)) {
                        return Ok(err(Value::Str(format!(
                            "sql.open: `{path}` outside --allow-fs-write"))));
                    }
                }
                match rusqlite::Connection::open(&path) {
                    Ok(conn) => {
                        let handle = next_sql_handle();
                        sql_registry().lock().unwrap().insert(handle, conn);
                        Ok(ok(Value::Int(handle as i64)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("sql.open: {e}")))),
                }
            }
            ("sql", "close") => {
                let h = expect_sql_handle(args.first())?;
                sql_registry().lock().unwrap().remove(h);
                Ok(Value::Unit)
            }
            ("sql", "exec") => {
                let h = expect_sql_handle(args.first())?;
                let stmt = expect_str(args.get(1))?.to_string();
                let params = expect_str_list(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.exec: closed or unknown Db handle".to_string())?;
                let conn = arc.lock().unwrap();
                let bind: Vec<&dyn rusqlite::ToSql> = params.iter()
                    .map(|s| s as &dyn rusqlite::ToSql)
                    .collect();
                match conn.execute(&stmt, rusqlite::params_from_iter(bind.iter())) {
                    Ok(n)  => Ok(ok(Value::Int(n as i64))),
                    Err(e) => Ok(err(Value::Str(format!("sql.exec: {e}")))),
                }
            }
            ("sql", "query") => {
                let h = expect_sql_handle(args.first())?;
                let stmt_str = expect_str(args.get(1))?.to_string();
                let params = expect_str_list(args.get(2))?;
                let arc = sql_registry().lock().unwrap()
                    .touch_get(h)
                    .ok_or_else(|| "sql.query: closed or unknown Db handle".to_string())?;
                let conn = arc.lock().unwrap();
                Ok(sql_run_query(&conn, &stmt_str, &params))
            }
            ("proc", "spawn") => {
                // The escape hatch effect. Spawns a child process,
                // collects its stdout/stderr, returns a structured
                // record. Allow-list is the binary basename: anything
                // outside `--allow-proc` is rejected pre-spawn.
                //
                // What this does NOT validate (per SECURITY.md):
                // - per-arg content (a script-like CLI invoked via
                //   --eval=... can run anything)
                // - environment variables (inherited from the parent)
                // - working directory (the parent's)
                //
                // For untrusted input, layer with OS-level
                // sandboxing — gVisor / nsjail / a container.
                let cmd = expect_str(args.first())?.to_string();
                let raw_args = match args.get(1) {
                    Some(Value::List(items)) => items,
                    Some(other) => return Err(format!(
                        "proc.spawn: args must be List[Str], got {other:?}")),
                    None => return Err("proc.spawn: missing args list".into()),
                };
                let str_args: Vec<String> = raw_args.iter().map(|v| match v {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(format!("proc.spawn: arg must be Str, got {other:?}")),
                }).collect::<Result<Vec<_>, _>>()?;

                // Allow-list check: empty list = any binary (escape
                // hatch); non-empty = basename of cmd must match an
                // entry exactly.
                if !self.policy.allow_proc.is_empty() {
                    let basename = std::path::Path::new(&cmd)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&cmd);
                    if !self.policy.allow_proc.iter().any(|a| a == basename) {
                        return Ok(err(Value::Str(format!(
                            "proc.spawn: `{cmd}` not in --allow-proc {:?}",
                            self.policy.allow_proc
                        ))));
                    }
                }

                // Hard caps: the spec doesn't pin numbers, but
                // unbounded argv is a DoS vector.
                if str_args.len() > 1024 {
                    return Ok(err(Value::Str(
                        "proc.spawn: arg-count exceeds 1024".into())));
                }
                if str_args.iter().any(|a| a.len() > 65_536) {
                    return Ok(err(Value::Str(
                        "proc.spawn: per-arg length exceeds 64 KiB".into())));
                }

                let output = std::process::Command::new(&cmd)
                    .args(&str_args)
                    .output();
                match output {
                    Ok(o) => {
                        let mut rec = indexmap::IndexMap::new();
                        rec.insert("stdout".into(), Value::Str(
                            String::from_utf8_lossy(&o.stdout).to_string()));
                        rec.insert("stderr".into(), Value::Str(
                            String::from_utf8_lossy(&o.stderr).to_string()));
                        rec.insert("exit_code".into(), Value::Int(
                            o.status.code().unwrap_or(-1) as i64));
                        Ok(ok(Value::Record(rec)))
                    }
                    Err(e) => Ok(err(Value::Str(format!("spawn `{cmd}`: {e}")))),
                }
            }
            other => Err(format!("unsupported effect {}.{}", other.0, other.1)),
        }
    }
}

/// Blocks the calling thread, accepts incoming HTTP requests on
/// `127.0.0.1:port`, and dispatches each through the named Lex
/// stage. Each request gets a fresh `Vm`; the program and policy
/// are shared.
///
/// Handler signature in Lex (by convention):
///   fn <name>(req :: Record { method :: Str, path :: Str, body :: Str })
///        -> Record { status :: Int, body :: Str }
/// PEM-encoded certificate + private key, both as raw bytes.
pub struct TlsConfig {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

fn serve_http(
    port: u16,
    handler_name: String,
    program: Arc<Program>,
    policy: Policy,
    tls: Option<TlsConfig>,
) -> Result<Value, String> {
    let (server, scheme) = match tls {
        None => (
            tiny_http::Server::http(("127.0.0.1", port))
                .map_err(|e| format!("net.serve bind {port}: {e}"))?,
            "http",
        ),
        Some(cfg) => {
            let ssl = tiny_http::SslConfig {
                certificate: cfg.cert,
                private_key: cfg.key,
            };
            (
                tiny_http::Server::https(("127.0.0.1", port), ssl)
                    .map_err(|e| format!("net.serve_tls bind {port}: {e}"))?,
                "https",
            )
        }
    };
    eprintln!("net.serve: listening on {scheme}://127.0.0.1:{port}");
    // Thread-per-request: the main loop accepts; each request runs on
    // its own worker thread with its own fresh Vm. The Program is
    // shared via Arc; Policy and handler_name are cloned per request.
    // Lex's immutability means there's no shared mutable state at the
    // language level — workers don't race.
    for req in server.incoming_requests() {
        let program = Arc::clone(&program);
        let policy = policy.clone();
        let handler_name = handler_name.clone();
        std::thread::spawn(move || handle_request(req, program, policy, handler_name));
    }
    Ok(Value::Unit)
}

fn handle_request(
    mut req: tiny_http::Request,
    program: Arc<Program>,
    policy: Policy,
    handler_name: String,
) {
    let lex_req = build_request_value(&mut req);
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&program));
    let mut vm = Vm::with_handler(&program, Box::new(handler));
    match vm.call(&handler_name, vec![lex_req]) {
        Ok(resp) => {
            let (status, body) = unpack_response(&resp);
            let response = tiny_http::Response::from_string(body).with_status_code(status);
            let _ = req.respond(response);
        }
        Err(e) => {
            let response = tiny_http::Response::from_string(format!("internal error: {e}"))
                .with_status_code(500);
            let _ = req.respond(response);
        }
    }
}

fn build_request_value(req: &mut tiny_http::Request) -> Value {
    let method = format!("{:?}", req.method()).to_uppercase();
    let url = req.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url, String::new()),
    };
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let mut rec = indexmap::IndexMap::new();
    rec.insert("method".into(), Value::Str(method));
    rec.insert("path".into(), Value::Str(path));
    rec.insert("query".into(), Value::Str(query));
    rec.insert("body".into(), Value::Str(body));
    Value::Record(rec)
}

fn unpack_response(v: &Value) -> (u16, String) {
    if let Value::Record(rec) = v {
        let status = rec.get("status").and_then(|s| match s {
            Value::Int(n) => Some(*n as u16),
            _ => None,
        }).unwrap_or(200);
        let body = rec.get("body").and_then(|b| match b {
            Value::Str(s) => Some(s.clone()),
            _ => None,
        }).unwrap_or_default();
        return (status, body);
    }
    (500, format!("handler returned non-record: {v:?}"))
}

/// HTTP/1.1 client backed by `ureq` + `rustls`. Accepts both
/// `http://` and `https://` URLs. Returns `Result[Str, Str]` as a
/// Lex `Value::Variant`. The earlier hand-rolled HTTP/1.0 client
/// was plain-TCP only — most public APIs are HTTPS, so the demo
/// could fetch `example.com` but not `wttr.in` or `api.github.com`.
fn http_request(method: &str, url: &str, body: Option<&str>) -> Value {
    use std::time::Duration;
    // ureq 3 puts 4xx/5xx behind `Error::StatusCode(code)` and consumes
    // the response, so the body would be lost. Disabling
    // `http_status_as_error` lets us check the status manually and
    // surface `Err("status 404: <body>")` like the old code did.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .timeout_send_body(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let resp = match (method, body) {
        ("GET", _) => agent.get(url).call(),
        ("POST", Some(b)) => agent.post(url).send(b),
        ("POST", None) => agent.post(url).send(""),
        (m, _) => return err_value(format!("unsupported method: {m}")),
    };
    match resp {
        Ok(mut r) => {
            let status = r.status().as_u16();
            let body = r.body_mut().read_to_string().unwrap_or_default();
            if (200..300).contains(&status) {
                Value::Variant { name: "Ok".into(), args: vec![Value::Str(body)] }
            } else {
                err_value(format!("status {status}: {body}"))
            }
        }
        Err(e) => err_value(format!("transport: {e}")),
    }
}

/// Build a ureq agent for `std.http.{send,get,post}` with the given
/// timeout (None → use the same defaults as the legacy `net.{get,post}`
/// path). Separate from `http_request` so the rich `http.send` flow
/// can supply per-request overrides.
fn http_agent(timeout_ms: Option<u64>) -> ureq::Agent {
    use std::time::Duration;
    let mut b = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .timeout_send_body(Some(Duration::from_secs(10)))
        .http_status_as_error(false);
    if let Some(ms) = timeout_ms {
        let d = Duration::from_millis(ms);
        b = b.timeout_global(Some(d));
    }
    b.build().into()
}

/// Map ureq's transport error to the structured `HttpError` variant
/// std.http exposes to user code. Anything not specifically a
/// timeout / TLS error funnels into `NetworkError`.
fn http_error_value(e: ureq::Error) -> Value {
    let (ctor, payload): (&str, Option<String>) = match &e {
        ureq::Error::Timeout(_) => ("TimeoutError", None),
        ureq::Error::Tls(s) => ("TlsError", Some((*s).into())),
        ureq::Error::Pem(p) => ("TlsError", Some(format!("{p}"))),
        ureq::Error::Rustls(r) => ("TlsError", Some(format!("{r}"))),
        _ => ("NetworkError", Some(format!("{e}"))),
    };
    let args = match payload { Some(s) => vec![Value::Str(s)], None => vec![] };
    let inner = Value::Variant { name: ctor.into(), args };
    Value::Variant { name: "Err".into(), args: vec![inner] }
}

fn http_decode_err(msg: String) -> Value {
    let inner = Value::Variant {
        name: "DecodeError".into(),
        args: vec![Value::Str(msg)],
    };
    Value::Variant { name: "Err".into(), args: vec![inner] }
}

/// Run a request and pack the ureq response into the
/// `{ status, headers, body }` Lex record (or the structured
/// `HttpError` on failure). `headers_extra` pairs are appended to the
/// outgoing request after `content_type` is applied.
fn http_send_simple(
    method: &str,
    url: &str,
    body: Option<Vec<u8>>,
    content_type: &str,
    timeout_ms: Option<u64>,
) -> Value {
    http_send_full(method, url, body, content_type, &[], timeout_ms)
}

fn http_send_full(
    method: &str,
    url: &str,
    body: Option<Vec<u8>>,
    content_type: &str,
    headers: &[(String, String)],
    timeout_ms: Option<u64>,
) -> Value {
    let agent = http_agent(timeout_ms);
    let resp = match method {
        "GET" => {
            let mut req = agent.get(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.call()
        }
        "POST" => {
            let body = body.unwrap_or_default();
            let mut req = agent.post(url);
            if !content_type.is_empty() { req = req.header("content-type", content_type); }
            for (k, v) in headers { req = req.header(k.as_str(), v.as_str()); }
            req.send(&body[..])
        }
        m => {
            // Other methods (PUT, DELETE, PATCH, ...) fall through
            // here in v1.5; for now surface a structured DecodeError
            // so the caller can match it.
            return http_decode_err(format!("unsupported method: {m}"));
        }
    };
    match resp {
        Ok(mut r) => {
            let status = r.status().as_u16() as i64;
            let headers_map = collect_response_headers(r.headers());
            let body_bytes = match r.body_mut().with_config().limit(10 * 1024 * 1024).read_to_vec() {
                Ok(b) => b,
                Err(e) => return http_decode_err(format!("body read: {e}")),
            };
            let mut rec = indexmap::IndexMap::new();
            rec.insert("status".into(), Value::Int(status));
            rec.insert("headers".into(), Value::Map(headers_map));
            rec.insert("body".into(), Value::Bytes(body_bytes));
            Value::Variant { name: "Ok".into(), args: vec![Value::Record(rec)] }
        }
        Err(e) => http_error_value(e),
    }
}

fn collect_response_headers(
    headers: &ureq::http::HeaderMap,
) -> std::collections::BTreeMap<lex_bytecode::MapKey, Value> {
    let mut out = std::collections::BTreeMap::new();
    for (name, value) in headers.iter() {
        let v = value.to_str().unwrap_or("").to_string();
        out.insert(lex_bytecode::MapKey::Str(name.as_str().to_string()), Value::Str(v));
    }
    out
}

/// Pull the standard `HttpRequest` shape out of a `Value::Record`
/// and dispatch through `http_send_full`. The handler verifies
/// `--allow-net-host` for the URL before sending.
fn http_send_record(handler: &DefaultHandler, req: &indexmap::IndexMap<String, Value>) -> Value {
    let method = match req.get("method") {
        Some(Value::Str(s)) => s.clone(),
        _ => return http_decode_err("HttpRequest.method must be Str".into()),
    };
    let url = match req.get("url") {
        Some(Value::Str(s)) => s.clone(),
        _ => return http_decode_err("HttpRequest.url must be Str".into()),
    };
    if let Err(e) = handler.ensure_host_allowed(&url) {
        return http_decode_err(e);
    }
    let body = match req.get("body") {
        Some(Value::Variant { name, args }) if name == "None" => None,
        Some(Value::Variant { name, args }) if name == "Some" => match args.as_slice() {
            [Value::Bytes(b)] => Some(b.clone()),
            _ => return http_decode_err("HttpRequest.body Some payload must be Bytes".into()),
        },
        _ => return http_decode_err("HttpRequest.body must be Option[Bytes]".into()),
    };
    let timeout_ms = match req.get("timeout_ms") {
        Some(Value::Variant { name, .. }) if name == "None" => None,
        Some(Value::Variant { name, args }) if name == "Some" => match args.as_slice() {
            [Value::Int(n)] if *n >= 0 => Some(*n as u64),
            _ => return http_decode_err(
                "HttpRequest.timeout_ms Some payload must be a non-negative Int".into()),
        },
        _ => return http_decode_err("HttpRequest.timeout_ms must be Option[Int]".into()),
    };
    let headers: Vec<(String, String)> = match req.get("headers") {
        Some(Value::Map(m)) => m.iter().filter_map(|(k, v)| {
            let kk = match k { lex_bytecode::MapKey::Str(s) => s.clone(), _ => return None };
            let vv = match v { Value::Str(s) => s.clone(), _ => return None };
            Some((kk, vv))
        }).collect(),
        _ => return http_decode_err("HttpRequest.headers must be Map[Str, Str]".into()),
    };
    http_send_full(&method, &url, body, "", &headers, timeout_ms)
}

fn expect_record(v: Option<&Value>) -> Result<&indexmap::IndexMap<String, Value>, String> {
    match v {
        Some(Value::Record(r)) => Ok(r),
        Some(other) => Err(format!("expected Record, got {other:?}")),
        None => Err("missing Record argument".into()),
    }
}

fn err_value(msg: String) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(msg)] }
}

fn expect_str(v: Option<&Value>) -> Result<&str, String> {
    match v {
        Some(Value::Str(s)) => Ok(s),
        Some(other) => Err(format!("expected Str arg, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_int(v: Option<&Value>) -> Result<i64, String> {
    match v {
        Some(Value::Int(n)) => Ok(*n),
        Some(other) => Err(format!("expected Int arg, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn ok(v: Value) -> Value {
    Value::Variant { name: "Ok".into(), args: vec![v] }
}
fn err(v: Value) -> Value {
    Value::Variant { name: "Err".into(), args: vec![v] }
}

impl DefaultHandler {
    /// Implementation of `agent.call_mcp(server, tool, args_json)`.
    /// Goes through the LRU client cache (#197): the named server
    /// is spawned on first use and reused on subsequent calls.
    /// On failure the offending client is dropped so the next
    /// call respawns rather than silently failing forever.
    fn dispatch_call_mcp(&mut self, args: Vec<Value>) -> Value {
        let server = match args.first() {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): server must be Str".into())),
        };
        let tool = match args.get(1) {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): tool must be Str".into())),
        };
        let args_json = match args.get(2) {
            Some(Value::Str(s)) => s.clone(),
            _ => return err(Value::Str(
                "agent.call_mcp(server, tool, args_json): args_json must be Str".into())),
        };
        let parsed: serde_json::Value = match serde_json::from_str(&args_json) {
            Ok(v) => v,
            Err(e) => return err(Value::Str(format!(
                "agent.call_mcp: args_json is not valid JSON: {e}"))),
        };
        match self.mcp_clients.call(&server, &tool, parsed) {
            Ok(result) => ok(Value::Str(
                serde_json::to_string(&result).unwrap_or_else(|_| "null".into()))),
            Err(e) => err(Value::Str(e)),
        }
    }
}

/// Implementation of `agent.local_complete(prompt)` (#196).
/// Hits Ollama (or any compatible HTTP service via `OLLAMA_HOST`)
/// and returns the completion text. Override at the
/// `EffectHandler` layer if you need a different transport.
fn dispatch_llm_local(args: Vec<Value>) -> Value {
    let prompt = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return err(Value::Str(
            "agent.local_complete(prompt): prompt must be Str".into())),
    };
    match crate::llm::local_complete(&prompt) {
        Ok(text) => ok(Value::Str(text)),
        Err(e) => err(Value::Str(e)),
    }
}

/// Implementation of `agent.cloud_complete(prompt)` (#196).
/// Hits OpenAI's chat-completions API (or any compatible
/// service via `OPENAI_BASE_URL`) and returns the assistant
/// message. Requires `OPENAI_API_KEY`. Override at the
/// `EffectHandler` layer for custom auth, batching, or other
/// providers.
fn dispatch_llm_cloud(args: Vec<Value>) -> Value {
    let prompt = match args.first() {
        Some(Value::Str(s)) => s.clone(),
        _ => return err(Value::Str(
            "agent.cloud_complete(prompt): prompt must be Str".into())),
    };
    match crate::llm::cloud_complete(&prompt) {
        Ok(text) => ok(Value::Str(text)),
        Err(e) => err(Value::Str(e)),
    }
}

fn some(v: Value) -> Value {
    Value::Variant { name: "Some".into(), args: vec![v] }
}
fn none() -> Value {
    Value::Variant { name: "None".into(), args: vec![] }
}

fn expect_bytes(v: Option<&Value>) -> Result<&Vec<u8>, String> {
    match v {
        Some(Value::Bytes(b)) => Ok(b),
        Some(other) => Err(format!("expected Bytes arg, got {other:?}")),
        None => Err("missing argument".into()),
    }
}

fn expect_kv_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected Kv handle (Int), got {other:?}")),
        None => Err("missing Kv argument".into()),
    }
}

fn expect_sql_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected Db handle (Int), got {other:?}")),
        None => Err("missing Db argument".into()),
    }
}

fn expect_str_list(v: Option<&Value>) -> Result<Vec<String>, String> {
    match v {
        Some(Value::List(items)) => items.iter().map(|x| match x {
            Value::Str(s) => Ok(s.clone()),
            other => Err(format!("expected List[Str] element, got {other:?}")),
        }).collect(),
        Some(other) => Err(format!("expected List[Str], got {other:?}")),
        None => Err("missing List[Str] argument".into()),
    }
}

/// Run a `SELECT` (or any returning statement) and pack the rows
/// into `Value::List(Value::Record(...))` shape — column-name keys,
/// SQLite-typed values mapped one-for-one to Lex value variants
/// (Null → Unit, Integer → Int, Real → Float, Text → Str, Blob →
/// Bytes). Returns the standard `Result[List[T], Str]` Lex shape.
fn sql_run_query(
    conn: &rusqlite::Connection,
    stmt_str: &str,
    params: &[String],
) -> Value {
    let mut stmt = match conn.prepare(stmt_str) {
        Ok(s)  => s,
        Err(e) => return err(Value::Str(format!("sql.query: {e}"))),
    };
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bind: Vec<&dyn rusqlite::ToSql> = params.iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let mut rows = match stmt.query(rusqlite::params_from_iter(bind.iter())) {
        Ok(r)  => r,
        Err(e) => return err(Value::Str(format!("sql.query: {e}"))),
    };
    let mut out: Vec<Value> = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None)    => break,
            Err(e)      => return err(Value::Str(format!("sql.query: {e}"))),
        };
        let mut rec = indexmap::IndexMap::new();
        for (i, name) in column_names.iter().enumerate() {
            let cell = match row.get_ref(i) {
                Ok(c)  => sql_value_ref_to_lex(c),
                Err(e) => return err(Value::Str(format!("sql.query: column {i}: {e}"))),
            };
            rec.insert(name.clone(), cell);
        }
        out.push(Value::Record(rec));
    }
    ok(Value::List(out))
}

fn sql_value_ref_to_lex(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null       => Value::Unit,
        ValueRef::Integer(n) => Value::Int(n),
        ValueRef::Real(f)    => Value::Float(f),
        ValueRef::Text(s)    => Value::Str(String::from_utf8_lossy(s).into_owned()),
        ValueRef::Blob(b)    => Value::Bytes(b.to_vec()),
    }
}

// -- log state (process-wide; configurable via log.set_*) --

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum LogLevel { Debug, Info, Warn, Error }

#[derive(Clone, Copy, PartialEq)]
enum LogFormat { Text, Json }

#[derive(Clone)]
enum LogSink {
    Stderr,
    File(std::sync::Arc<Mutex<std::fs::File>>),
}

struct LogState {
    level: LogLevel,
    format: LogFormat,
    sink: LogSink,
}

fn log_state() -> &'static Mutex<LogState> {
    static STATE: OnceLock<Mutex<LogState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(LogState {
        level: LogLevel::Info,
        format: LogFormat::Text,
        sink: LogSink::Stderr,
    }))
}

fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        _ => None,
    }
}

fn level_label(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

fn emit_log(level: LogLevel, msg: &str) {
    let state = log_state().lock().unwrap();
    if level < state.level {
        return;
    }
    let ts = chrono::Utc::now().to_rfc3339();
    let line = match state.format {
        LogFormat::Text => format!("[{}] {}: {}\n", ts, level_label(level), msg),
        LogFormat::Json => {
            // Hand-rolled JSON to avoid pulling serde_json into the
            // hot path; msg gets minimal escaping (the four common
            // cases that break a JSON line).
            let escaped = msg
                .replace('\\', "\\\\")
                .replace('"',  "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            format!(
                "{{\"ts\":\"{ts}\",\"level\":\"{}\",\"msg\":\"{escaped}\"}}\n",
                level_label(level),
            )
        }
    };
    let sink = state.sink.clone();
    drop(state);
    match sink {
        LogSink::Stderr => {
            use std::io::Write;
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        LogSink::File(f) => {
            use std::io::Write;
            if let Ok(mut g) = f.lock() {
                let _ = g.write_all(line.as_bytes());
            }
        }
    }
}

pub(crate) struct ProcessState {
    child: std::process::Child,
    stdout: Option<std::io::BufReader<std::process::ChildStdout>>,
    stderr: Option<std::io::BufReader<std::process::ChildStderr>>,
}

/// Process-wide registry of live `process.spawn` handles. Capped at
/// [`MAX_PROCESS_HANDLES`] to bound long-running programs that spawn
/// many short-lived children: on each `spawn` past the cap, the
/// least-recently-used entry is dropped (which `Drop`s its
/// `ProcessState`, leaving the child orphaned but the registry
/// bounded). `process.wait` also drops the entry on completion since
/// the handle becomes terminal once the child exits.
///
/// Each entry is wrapped in `Arc<Mutex<ProcessState>>` so the global
/// lookup mutex is held only briefly during dispatch — once we have
/// the per-handle `Arc`, the global lock is released and the slow
/// op (`wait`, `read_*_line`) only contends on its own handle's
/// mutex. Reads on different handles no longer block each other.
fn process_registry() -> &'static Mutex<ProcessRegistry> {
    static REGISTRY: OnceLock<Mutex<ProcessRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(ProcessRegistry::with_capacity(MAX_PROCESS_HANDLES)))
}

const MAX_PROCESS_HANDLES: usize = 256;

type SharedProcessState = Arc<Mutex<ProcessState>>;

pub(crate) struct ProcessRegistry {
    entries: indexmap::IndexMap<u64, SharedProcessState>,
    cap: usize,
}

impl ProcessRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    /// Insert a freshly-spawned child. If at cap, evict the LRU entry
    /// first; the dropped `ProcessState`'s child stays alive (orphaned)
    /// but its file descriptors are released.
    pub(crate) fn insert(&mut self, handle: u64, state: ProcessState) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(state)));
    }

    /// Look up a handle, marking it most-recently-used on hit. Returns
    /// a clone of the shared `Arc` — callers should release the global
    /// registry lock before locking the per-handle mutex.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<SharedProcessState> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    /// Drop the registry entry. The underlying `Arc` may outlive the
    /// removal if another op still holds it; that's intentional — the
    /// in-flight op finishes against the existing `ProcessState`, and
    /// only fresh lookups start failing.
    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

fn next_process_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(all(test, unix))]
mod process_registry_tests {
    use super::{ProcessRegistry, ProcessState};

    /// Spawn a trivial short-lived child for use as registry payload.
    /// `true` exits immediately — we don't actually run the child for
    /// real, we just need a valid `std::process::Child`.
    fn fresh_state() -> ProcessState {
        let child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn `true`");
        ProcessState { child, stdout: None, stderr: None }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn touch_get_returns_distinct_arcs_for_distinct_handles() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        r.insert(2, fresh_state());
        let a = r.touch_get(1).unwrap();
        let b = r.touch_get(2).unwrap();
        // Different Arcs — pointer-equality check.
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        let mut r = ProcessRegistry::with_capacity(2);
        r.insert(1, fresh_state());
        r.insert(2, fresh_state());
        let _ = r.touch_get(1);
        r.insert(3, fresh_state());
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted, should survive");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn cap_with_no_touches_evicts_in_insertion_order() {
        let mut r = ProcessRegistry::with_capacity(2);
        r.insert(10, fresh_state());
        r.insert(20, fresh_state());
        r.insert(30, fresh_state());
        assert!(r.touch_get(10).is_none());
        assert!(r.touch_get(20).is_some());
        assert!(r.touch_get(30).is_some());
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        let cap = 8;
        let mut r = ProcessRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh_state());
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }

    #[test]
    fn outstanding_arc_outlives_remove() {
        // Holding the per-handle Arc while another op removes the
        // entry must not invalidate the in-flight op. Mirrors the
        // wait-completes-then-removes pattern.
        let mut r = ProcessRegistry::with_capacity(4);
        r.insert(1, fresh_state());
        let arc = r.touch_get(1).expect("entry exists");
        r.remove(1);
        // Registry forgot about it, but the Arc still works.
        assert!(r.touch_get(1).is_none());
        let _state = arc.lock().unwrap();
    }
}

fn expect_process_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected ProcessHandle (Int), got {other:?}")),
        None => Err("missing ProcessHandle argument".into()),
    }
}

/// Process-wide registry of open `Kv` handles. Each `kv.open` allocates
/// a new u64 handle via [`next_kv_handle`] and stores the `sled::Db`
/// here; subsequent ops fetch by handle. `kv.close` removes the entry.
///
/// Capped at [`MAX_KV_HANDLES`] to prevent leaks from long-running
/// programs that open many short-lived stores without calling
/// `kv.close`. On insert at cap, the least-recently-used entry is
/// dropped (closing its `sled::Db`); subsequent ops on the evicted
/// handle return the standard "closed or unknown Kv handle" error.
/// Any access (`get`, `put`, `delete`, `contains`, `list_prefix`)
/// touches the LRU order.
fn kv_registry() -> &'static Mutex<KvRegistry> {
    static REGISTRY: OnceLock<Mutex<KvRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(KvRegistry::with_capacity(MAX_KV_HANDLES)))
}

/// Maximum number of `kv.open` handles kept alive at once. Past this
/// cap, the least-recently-used handle is evicted on each new open.
/// Sized so that pathological "open and forget" programs are bounded
/// without breaking real-world programs that intentionally keep one or
/// two long-lived stores open.
const MAX_KV_HANDLES: usize = 256;

/// LRU-bounded set of open `sled::Db` instances keyed by `u64` handle.
/// Built on `IndexMap` for O(1) insert / remove / lookup with
/// insertion-order traversal — touching an entry just shift-moves it
/// to the back, evictions pop from the front.
pub(crate) struct KvRegistry {
    entries: indexmap::IndexMap<u64, sled::Db>,
    cap: usize,
}

impl KvRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    /// Insert a freshly-opened db. If we're already at cap, evict the
    /// LRU entry first; the dropped `sled::Db` closes its files.
    pub(crate) fn insert(&mut self, handle: u64, db: sled::Db) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, db);
    }

    /// Look up a handle, marking it most-recently-used on hit.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<&sled::Db> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle)
    }

    /// Explicit `kv.close`: drop the handle if present.
    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

fn next_kv_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Process-wide registry of open `Db` handles. Same shape as the kv
/// and process registries: per-handle `Arc<Mutex<…>>` so dispatch
/// only briefly holds the global lock and ops on different
/// connections don't serialize. LRU-bounded at
/// [`MAX_SQL_HANDLES`] to avoid leaks from long-running programs
/// that open many short-lived databases.
fn sql_registry() -> &'static Mutex<SqlRegistry> {
    static REGISTRY: OnceLock<Mutex<SqlRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SqlRegistry::with_capacity(MAX_SQL_HANDLES)))
}

const MAX_SQL_HANDLES: usize = 256;

type SharedConn = Arc<Mutex<rusqlite::Connection>>;

pub(crate) struct SqlRegistry {
    entries: indexmap::IndexMap<u64, SharedConn>,
    cap: usize,
}

impl SqlRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(crate) fn insert(&mut self, handle: u64, conn: rusqlite::Connection) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(conn)));
    }

    /// Look up a handle, marking it MRU on hit. Returns a clone of
    /// the shared `Arc` so callers release the global registry
    /// lock before locking the per-handle mutex.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<SharedConn> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

fn next_sql_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod sql_registry_tests {
    use super::SqlRegistry;

    fn fresh() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().expect("open in-memory sqlite")
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = SqlRegistry::with_capacity(4);
        r.insert(1, fresh());
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        let mut r = SqlRegistry::with_capacity(2);
        r.insert(1, fresh());
        r.insert(2, fresh());
        let _ = r.touch_get(1);
        r.insert(3, fresh());
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = SqlRegistry::with_capacity(4);
        r.insert(1, fresh());
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        let cap = 8;
        let mut r = SqlRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh());
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }
}

#[cfg(test)]
mod kv_registry_tests {
    use super::KvRegistry;

    /// Spin up an isolated `sled::Db` in a temp dir. Each call gets a
    /// unique path so concurrent tests don't collide on the lockfile.
    fn fresh_db(tag: &str) -> sled::Db {
        let dir = std::env::temp_dir().join(format!(
            "lex-kv-reg-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        sled::open(&dir).expect("sled open")
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("a"));
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        // cap=2: insert 1, 2; touch 1 (now MRU); insert 3 → 2 evicted.
        let mut r = KvRegistry::with_capacity(2);
        r.insert(1, fresh_db("c1"));
        r.insert(2, fresh_db("c2"));
        let _ = r.touch_get(1);
        r.insert(3, fresh_db("c3"));
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted, should survive");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn cap_with_no_touches_evicts_in_insertion_order() {
        // cap=2: insert 1, 2, 3 with no touches → 1 evicted (FIFO).
        let mut r = KvRegistry::with_capacity(2);
        r.insert(10, fresh_db("f1"));
        r.insert(20, fresh_db("f2"));
        r.insert(30, fresh_db("f3"));
        assert!(r.touch_get(10).is_none());
        assert!(r.touch_get(20).is_some());
        assert!(r.touch_get(30).is_some());
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("r1"));
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn remove_unknown_handle_is_noop() {
        let mut r = KvRegistry::with_capacity(4);
        r.insert(1, fresh_db("u1"));
        r.remove(999);
        assert!(r.touch_get(1).is_some());
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        // Exhaust the cap to confirm the registry never grows past it,
        // even under sustained churn.
        let cap = 8;
        let mut r = KvRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh_db(&format!("b{i}")));
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }
}
